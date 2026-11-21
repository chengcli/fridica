//! System reader for placement load probes: the fixed probe script, run locally
//! or over SSH with the same scrubbed environment and read-only SSH options as
//! the readiness probes. Output is bounded; failures yield no reading.
use crate::{
    config::{registry::Machine, Config},
    core::delivery::AdapterFuture,
    exec::{probe::Control, process},
    machines::probe::{self, Reader, Reading},
};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};

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
        if !matches!(machine.transport.as_str(), "local" | "ssh") {
            // Slurm and other transports are not probed; placement ignores them.
            return None;
        }
        // The control directory is held until the probe finishes.
        let (launch, control) = crate::exec::probe::launch(
            machine,
            &self.home,
            self.excluded_env.clone(),
            self.environment.clone(),
            args,
            Control::ReadOnly("fridica-load-"),
        )
        .ok()?;
        let completed = process::run_once(launch, vec![], timeout, probe::OUTPUT_LIMIT)
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
