//! Confined-worker boundary. Ordinary unrestricted workers keep the
//! trusted-owner model; this helper masks daemon state before a backend starts.
use super::{
    local::LocalTransport,
    process::{self, Launch},
    shell,
    ssh::{LaunchOptions, SshTransport},
};
use crate::config::{registry::Machine, Config};
use anyhow::{bail, Result};
use serde_json::json;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Component, Path, PathBuf},
};

pub const HELPER: &str = include_str!("isolation_helper.py");
#[derive(Clone)]
pub struct Isolation {
    private: Vec<PathBuf>,
    remote: BTreeMap<String, RemoteFiles>,
}
#[derive(Clone)]
struct RemoteFiles {
    host: String,
    private: Vec<String>,
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
        private.extend(config.parent.repos.iter().cloned());
        private.extend_from_slice(additional);
        if private
            .iter()
            .any(|p| !safe_path(p) || p.parent() == Some(Path::new("/")))
        {
            bail!("worker isolation requires private files below dedicated directories");
        }
        Ok(Self {
            private,
            remote: BTreeMap::new(),
        })
    }
    /// Trusted construction must inventory ALL private files visible on this
    /// target, including shared daemon paths and remote control/capability files.
    /// Paths are absolute or relative to the remote owner's home (`~/`). Their
    /// parent directories must exist. Local paths are never guessed or forwarded.
    /// Until provisioning supplies this inventory, confined SSH fails closed.
    pub fn with_remote_files(mut self, machine: &Machine, private: &[String]) -> Result<Self> {
        if machine.transport != "ssh"
            || !crate::config::registry::ssh_host(&machine.host)
            || private.is_empty()
            || private
                .iter()
                .any(|p| !safe_remote_path(p) || Path::new(p).parent() == Some(Path::new("/")))
            || self.remote.contains_key(&machine.name)
        {
            bail!("worker isolation requires a unique SSH target and private-file inventory");
        }
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
        let Some(profile) = self.remote.get(&transport.machine.name) else {
            bail!("confined SSH requires a target private-file inventory");
        };
        if profile.host != transport.machine.host || !safe_remote_path(cwd) {
            bail!("confined SSH target or workspace does not match its inventory");
        }
        shell::validate(&command)?;
        let mut argv = vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-S".into(),
            "-c".into(),
            HELPER.into(),
            json!({"home":null,"workspace":cwd,"private":profile.private,
                "create":create,"excluded_env":transport.excluded_env})
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
            HELPER.into(),
            json!({"home":transport.home,"workspace":cwd,"private":self.private,"create":create})
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
fn safe_remote_path(value: &str) -> bool {
    let expanded = value
        .strip_prefix("~/")
        .map(|tail| format!("/remote-home/{tail}"));
    let path = Path::new(expanded.as_deref().unwrap_or(value));
    safe_path(path)
        && path != Path::new("/")
        && !value.ends_with('/')
        && !value.split('/').any(|part| part == "." || part == "..")
}
fn safe_path(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some_and(|p| !p.contains('\0'))
        && path
            .components()
            .all(|p| matches!(p, Component::RootDir | Component::Normal(_)))
}
