//! Confined-worker boundary. Ordinary unrestricted workers keep the
//! trusted-owner model; this helper masks daemon state before a backend starts.
use super::{
    local::LocalTransport,
    process::{self, Launch},
    shell,
    ssh::{LaunchOptions, SshTransport},
};
use crate::config::{
    isolation::{remote_path, validate_identities, validate_remote_files},
    registry::Machine,
    Config,
};
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
    include_str!("isolation_settings.py"),
    "\n",
    include_str!("isolation_helper.py")
);
const BOOTSTRAP: &str = include_str!("isolation_bootstrap.py");
#[derive(Clone, PartialEq, Eq)]
pub struct Isolation {
    private: Vec<PathBuf>,
    remote: BTreeMap<String, RemoteFiles>,
    mcp_aliases: Vec<String>,
    mcp_urls: Vec<String>,
}
#[derive(Clone, PartialEq, Eq)]
struct RemoteFiles {
    host: String,
    private: Vec<String>,
}
impl Isolation {
    /// Additional capability files must be supplied by trusted daemon
    /// construction; they cannot come from a job or a model response.
    pub fn new(config: &Config, additional: &[PathBuf]) -> Result<Self> {
        config.isolation.validate(&config.machines)?;
        let mut private = vec![
            config.path.clone(),
            config.state.path.clone(),
            config.state.control_socket.clone(),
        ];
        private.extend(config.owner.contract.iter().cloned());
        private.extend(config.parent.repos.iter().cloned());
        private.extend_from_slice(additional);
        private.extend(config.isolation.private_files.iter().cloned());
        if private
            .iter()
            .any(|p| !safe_path(p) || p.parent() == Some(Path::new("/")))
        {
            bail!("worker isolation requires private files below dedicated directories");
        }
        let mut isolation = Self {
            private,
            remote: BTreeMap::new(),
            mcp_aliases: vec![],
            mcp_urls: vec![],
        }
        .with_mcp_identities(&config.isolation.mcp_aliases, &config.isolation.mcp_urls)?;
        for (name, inventory) in &config.isolation.remote {
            let machine = config
                .machines
                .get(name)
                .expect("validated inventory machine");
            isolation = isolation.with_remote_files(machine, &inventory.private_files)?;
        }
        Ok(isolation)
    }
    /// Owner-provisioned identities for wrappers and HTTP servers that cannot
    /// be identified from a direct Fridica executable or environment reference.
    /// Endpoints must not contain credentials: these identities travel in argv.
    pub fn with_mcp_identities(mut self, aliases: &[String], urls: &[String]) -> Result<Self> {
        validate_identities(aliases, urls)?;
        self.mcp_aliases = aliases.to_vec();
        self.mcp_urls = urls.to_vec();
        Ok(self)
    }
    /// Trusted construction must inventory ALL private files visible on this
    /// target, including shared daemon paths and remote control/capability files.
    /// Paths are absolute or relative to the remote owner's home (`~/`). Their
    /// parent directories must exist. Local paths are never guessed or forwarded.
    /// Until provisioning supplies this inventory, confined SSH fails closed.
    pub fn with_remote_files(mut self, machine: &Machine, private: &[String]) -> Result<Self> {
        if machine.transport != "ssh"
            || !crate::config::registry::ssh_host(&machine.host)
            || self.remote.contains_key(&machine.name)
        {
            bail!("worker isolation requires a unique SSH target and private-file inventory");
        }
        validate_remote_files(private)?;
        self.remote.insert(
            machine.name.clone(),
            RemoteFiles {
                host: machine.host.clone(),
                private: private.to_vec(),
            },
        );
        Ok(self)
    }
    pub fn launch_remote(
        &self,
        transport: &SshTransport,
        command: Vec<String>,
        cwd: &str,
        inherited: BTreeMap<OsString, OsString>,
        create: bool,
    ) -> Result<Launch> {
        self.launch_remote_mode(transport, command, cwd, inherited, create, false)
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
    ) -> Result<Launch> {
        let Some(profile) = self.remote.get(&transport.machine.name) else {
            bail!("confined SSH requires a target private-file inventory");
        };
        if profile.host != transport.machine.host || !remote_path(cwd) {
            bail!("confined SSH target or workspace does not match its inventory");
        }
        shell::validate(&command)?;
        let mut argv = vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-S".into(),
            "-c".into(),
            BOOTSTRAP.into(),
            HELPER.into(),
            json!({"home":null,"workspace":cwd,"private":profile.private,
                "create":create,"preflight":preflight,"excluded_env":transport.excluded_env,
                "mcp_aliases":self.mcp_aliases,"mcp_urls":self.mcp_urls})
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
        self.launch_mode(transport, command, cwd, inherited, create, false)
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
            json!({"home":transport.home,"workspace":cwd,"private":self.private,"create":create,"preflight":preflight,
                "mcp_aliases":self.mcp_aliases,"mcp_urls":self.mcp_urls})
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
    MissingRemoteInventory,
    UnsupportedTransport,
    LaunchConfigurationRefused,
    InventoryRefused,
    SettingsRefused,
    NamespaceFailed,
    RuntimeOrTransportFailed,
    ProbeFailed,
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
