//! Local confined-worker boundary. Ordinary unrestricted workers keep the
//! trusted-owner model; this helper masks daemon state before a backend starts.
use super::{
    local::LocalTransport,
    process::{self, Launch},
    shell,
};
use crate::config::Config;
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
        Ok(Self { private })
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
fn safe_path(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some_and(|p| !p.contains('\0'))
        && path
            .components()
            .all(|p| matches!(p, Component::RootDir | Component::Normal(_)))
}
