//! System reader for placement load probes: the fixed probe script, run locally
//! or over SSH with the same scrubbed environment and read-only SSH options as
//! the readiness probes. Output is bounded; failures yield no reading.
use crate::{
    config::{registry::Machine, Config},
    core::delivery::AdapterFuture,
    exec::{
        isolation as helpers,
        local::LocalTransport,
        process,
        ssh::{LaunchOptions, SshTransport},
    },
    machines::probe::{self, Reader, Reading},
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct SystemReader {
    pub home: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub excluded_env: Vec<String>,
}
impl SystemReader {
    pub fn new(config: &Config, home: PathBuf, environment: BTreeMap<OsString, OsString>) -> Self {
        Self {
            home,
            environment,
            excluded_env: config.secret_env().iter().map(|s| s.to_string()).collect(),
        }
    }
    async fn run(&self, machine: &Machine, timeout: Duration) -> Option<Reading> {
        let args = vec!["/bin/sh".into(), "-c".into(), probe::SCRIPT.into()];
        // Held until the probe finishes; SSH needs a private control directory.
        let mut control = None;
        let launch = if machine.transport == "local" {
            LocalTransport {
                machine: machine.clone(),
                home: self.home.clone(),
                excluded_env: self.excluded_env.clone(),
            }
            .launch(
                args,
                Path::new("/"),
                self.environment.clone(),
                &BTreeMap::new(),
                None,
                false,
            )
        } else if machine.transport == "ssh" {
            let directory = tempfile::Builder::new()
                .prefix("fridica-load-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .ok()?;
            let launch = SshTransport {
                machine: machine.clone(),
                excluded_env: self.excluded_env.clone(),
                control_directory: directory.path().to_owned(),
            }
            .launch(
                args,
                "/",
                self.environment.clone(),
                &BTreeMap::new(),
                LaunchOptions::default(),
            )
            .map(|mut launch| {
                helpers::read_only_ssh_probe(&mut launch);
                launch
            });
            control = Some(directory);
            launch
        } else {
            // Slurm and other transports are not probed; placement ignores them.
            return None;
        };
        let completed = process::run_once(launch.ok()?, vec![], timeout, probe::OUTPUT_LIMIT)
            .await
            .ok()?;
        drop(control);
        (completed.returncode == 0)
            .then(|| probe::parse(&completed.stdout, 0.))
            .flatten()
    }
}
impl Reader for SystemReader {
    fn read<'a>(
        &'a self,
        machine: &'a Machine,
        timeout: Duration,
    ) -> AdapterFuture<'a, Option<Reading>> {
        Box::pin(self.run(machine, timeout))
    }
}
