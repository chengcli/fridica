//! Confined-worker boundary. Ordinary unrestricted workers keep the
//! trusted-owner model; this helper masks daemon state before a backend starts.
use super::{
    local::LocalTransport,
    process::{self, Launch},
    shell,
    ssh::{LaunchOptions, SshTransport},
};
use crate::config::{isolation::remote_path, Config};
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub const HELPER: &str = concat!(
    include_str!("settings_paths.py"),
    "\n",
    include_str!("isolation_settings.py"),
    "\n",
    include_str!("isolation_helper.py")
);
const MCP_STARTUP: &str = concat!(
    include_str!("settings_paths.py"),
    "\n",
    include_str!("isolation_settings.py"),
    "\n",
    include_str!("mcp_startup.py")
);
const READINESS: &str = concat!(
    include_str!("settings_paths.py"),
    "\n",
    include_str!("isolation_settings.py"),
    "\n",
    include_str!("readiness.py")
);
const BOOTSTRAP: &str = include_str!("isolation_bootstrap.py");
#[derive(Clone, PartialEq, Eq)]
pub struct Isolation {
    private: Vec<PathBuf>,
}
impl Isolation {
    /// Additional capability files must be supplied by trusted daemon
    /// construction; they cannot come from a job or a model response.
    pub fn new(config: &Config, additional: &[PathBuf]) -> Result<Self> {
        let mut private = vec![
            config.path.clone(),
            config.state.path.clone(),
            config.state.control_socket.clone(),
        ];
        private.extend(config.owner.contract.iter().cloned());
        // The egress deny list names what must stay private; workers never see it.
        private.extend(config.egress.deny_list.iter().cloned());
        private.extend(config.parent.repos.iter().cloned());
        private.extend_from_slice(additional);
        if private
            .iter()
            .any(|p| !safe_path(p) || p.parent() == Some(Path::new("/")))
        {
            bail!("worker isolation requires private files below dedicated directories");
        }
        Ok(Self { private })
    }
    /// Target-side discovery for unrestricted Codex: every MCP server in the
    /// target's Codex settings is disabled. No private-file inventory, mount
    /// namespace, copied state, or credentialed discovery command is used.
    pub fn mcp_startup(
        &self,
        command: Vec<String>,
        home: Option<&Path>,
        cwd: &Path,
        excluded_env: &[String],
        create: bool,
    ) -> Result<Vec<String>> {
        shell::validate(&command)?;
        if command.first().map(String::as_str) != Some("codex")
            || command.get(1).map(String::as_str) != Some("app-server")
            || !cwd.to_str().is_some_and(remote_path)
            || home.is_some_and(|p| !safe_path(p))
        {
            bail!("invalid Codex startup settings request");
        }
        let mut argv = vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-S".into(),
            "-c".into(),
            BOOTSTRAP.into(),
            MCP_STARTUP.into(),
            json!({"home":home,"workspace":cwd,"create":create,"excluded_env":excluded_env})
                .to_string(),
        ];
        argv.extend(command);
        Ok(argv)
    }

    /// Read-only helper arguments; caller selects local or SSH transport.
    /// Binary presence is checked without running a version/authentication command.
    pub fn readiness(
        &self,
        home: Option<&Path>,
        workspace: &Path,
        backend: &str,
        excluded_env: &[String],
        parent: bool,
    ) -> Result<Vec<String>> {
        if !["codex", "claude"].contains(&backend) {
            bail!("unknown readiness backend");
        }
        Ok(vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-S".into(),
            "-c".into(),
            BOOTSTRAP.into(),
            READINESS.into(),
            json!({"home":home,"workspace":workspace,"backend":backend,"parent":parent,"excluded_env":excluded_env})
                .to_string(),
        ])
    }

    pub fn launch_remote(
        &self,
        transport: &SshTransport,
        command: Vec<String>,
        cwd: &str,
        inherited: BTreeMap<OsString, OsString>,
        create: bool,
    ) -> Result<Launch> {
        self.launch_remote_mode(transport, command, cwd, inherited, create, false, None)
    }
    /// Read-only runtime probe through the same helper and transport as workers.
    /// No backend is executed and missing workspace directories are not created.
    pub fn preflight_remote(
        &self,
        transport: &SshTransport,
        cwd: &str,
        inherited: BTreeMap<OsString, OsString>,
    ) -> Result<Launch> {
        self.launch_remote_mode(
            transport,
            vec!["/bin/true".into()],
            cwd,
            inherited,
            false,
            true,
            None,
        )
    }
    pub fn preflight_backend_remote(
        &self,
        transport: &SshTransport,
        cwd: &str,
        inherited: BTreeMap<OsString, OsString>,
        backend: &str,
    ) -> Result<Launch> {
        self.launch_remote_mode(
            transport,
            vec!["/bin/true".into()],
            cwd,
            inherited,
            false,
            true,
            Some(backend),
        )
    }
    pub fn preflight_backend(
        &self,
        transport: &LocalTransport,
        cwd: &Path,
        inherited: BTreeMap<OsString, OsString>,
        backend: &str,
    ) -> Result<Launch> {
        self.launch_mode(
            transport,
            vec!["/bin/true".into()],
            cwd,
            inherited,
            false,
            true,
            Some(backend),
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn launch_remote_mode(
        &self,
        transport: &SshTransport,
        command: Vec<String>,
        cwd: &str,
        inherited: BTreeMap<OsString, OsString>,
        create: bool,
        preflight: bool,
        probe_backend: Option<&str>,
    ) -> Result<Launch> {
        // Workers keep the target's normal files (SSH keys, git and gh credentials)
        // so they can commit and push; confinement bounds writes and devices only.
        if !remote_path(cwd) {
            bail!("confined SSH workspace must be an absolute or home-relative path");
        }
        shell::validate(&command)?;
        let mut argv = vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-S".into(),
            "-c".into(),
            BOOTSTRAP.into(),
            HELPER.into(),
            json!({"home":null,"workspace":cwd,"private":[],
                "create":create,"preflight":preflight,"probe_backend":probe_backend,"excluded_env":transport.excluded_env})
            .to_string(),
        ];
        argv.extend(command);
        // Keep the SSH watchdog and its stdin channel. Only the fixed helper
        // creates/opens the workspace; no repository cwd or path-based mkdir.
        transport.launch(
            argv,
            "/",
            inherited,
            &BTreeMap::new(),
            LaunchOptions::default(),
        )
    }
    pub fn launch(
        &self,
        transport: &LocalTransport,
        command: Vec<String>,
        cwd: &Path,
        inherited: BTreeMap<OsString, OsString>,
        create: bool,
    ) -> Result<Launch> {
        self.launch_mode(transport, command, cwd, inherited, create, false, None)
    }
    /// Read-only runtime probe through the same helper and transport as workers.
    /// No backend is executed and missing workspace directories are not created.
    pub fn preflight(
        &self,
        transport: &LocalTransport,
        cwd: &Path,
        inherited: BTreeMap<OsString, OsString>,
    ) -> Result<Launch> {
        self.launch_mode(
            transport,
            vec!["/bin/true".into()],
            cwd,
            inherited,
            false,
            true,
            None,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn launch_mode(
        &self,
        transport: &LocalTransport,
        command: Vec<String>,
        cwd: &Path,
        inherited: BTreeMap<OsString, OsString>,
        create: bool,
        preflight: bool,
        probe_backend: Option<&str>,
    ) -> Result<Launch> {
        if !cfg!(target_os = "linux")
            || transport.machine.transport != "local"
            || !safe_path(cwd)
            || !safe_path(&transport.home)
        {
            bail!("local worker isolation requires Linux and absolute paths");
        }
        shell::validate(&command)?;
        let mut env = process::scrubbed_environment(
            inherited,
            &transport.excluded_env,
            &transport.machine.resources.environment(),
        );
        env.insert("HOME".into(), transport.home.clone().into_os_string());
        let mut argv = vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-S".into(),
            "-c".into(),
            BOOTSTRAP.into(),
            HELPER.into(),
            json!({"home":transport.home,"workspace":cwd,"private":self.private,"create":create,"preflight":preflight,"probe_backend":probe_backend})
                .to_string(),
        ];
        argv.extend(command);
        // No repository cwd, PYTHONPATH, shell expansion or repository-controlled
        // helper executable participates in preparing the mount namespace.
        Ok(Launch {
            argv,
            cwd: Some("/".into()),
            env,
        })
    }
}
fn safe_path(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some_and(|p| !p.contains('\0'))
        && path
            .components()
            .all(|p| matches!(p, Component::RootDir | Component::Normal(_)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    Passed,
    UnsupportedTransport,
    LaunchConfigurationRefused,
    InventoryRefused,
    SettingsRefused,
    NamespaceFailed,
    RuntimeOrTransportFailed,
    ProbeFailed,
    BackendMissing,
    WorkspaceRefused,
    HostPathsRefused,
    OwnerInputsRefused,
}
/// Bounded process ownership includes cancellation cleanup and the SSH stdin
/// watchdog. Exact markers and successful exit are both required for success.
pub async fn run_probe(launch: Launch, timeout: Duration) -> Check {
    probe_result(process::run_with_open_stdin(launch, timeout, 4096).await)
}

/// The cleanup owner retains probe capacity if admission is cancelled.
pub async fn run_probe_with_permit(
    launch: Launch,
    timeout: Duration,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Check {
    probe_result(process::run_with_open_stdin_with_permit(launch, timeout, 4096, permit).await)
}

fn probe_result(result: Result<process::Completed>) -> Check {
    let Ok(result) = result else {
        return Check::ProbeFailed;
    };
    match (result.returncode, result.stdout.as_slice()) {
        (0, b"fridica-isolation:namespace\nfridica-isolation:ready\n") => Check::Passed,
        (97, b"fridica-isolation:namespace\nfridica-isolation:backend-missing\n") => {
            Check::BackendMissing
        }
        (97, b"fridica-isolation:inventory-refused\n") => Check::InventoryRefused,
        (97, b"fridica-isolation:settings-refused\n") => Check::SettingsRefused,
        (_, b"fridica-isolation:namespace\n")
        | (97, b"fridica-isolation:namespace\nfridica-isolation:namespace-refused\n") => {
            Check::NamespaceFailed
        }
        _ => Check::RuntimeOrTransportFailed,
    }
}

/// First option wins in OpenSSH: probes must not reuse/persist a master or
/// enroll a host key. Shared by explicit doctor and automatic job admission.
pub fn read_only_ssh_probe(launch: &mut Launch) {
    launch.argv.splice(
        1..1,
        [
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ControlPersist=no",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "UpdateHostKeys=no",
        ]
        .map(str::to_owned),
    );
}

/// Same bounded process ownership as namespace probes, with fixed diagnostics.
pub async fn run_readiness(launch: Launch, timeout: Duration) -> Check {
    let Ok(result) = process::run_with_open_stdin(launch, timeout, 4096).await else {
        return Check::ProbeFailed;
    };
    match (result.returncode, result.stdout.as_slice()) {
        (0, b"fridica-readiness:ready\n") => Check::Passed,
        (97, b"fridica-readiness:backend-missing\n") => Check::BackendMissing,
        (97, b"fridica-readiness:workspace-refused\n") => Check::WorkspaceRefused,
        (97, b"fridica-readiness:settings-refused\n") => Check::SettingsRefused,
        _ => Check::RuntimeOrTransportFailed,
    }
}
