//! Bounded SSH artifact reads with target-side confinement before transfer.
use super::{
    local::ARTIFACT_LIMIT,
    process,
    ssh::{LaunchOptions, SshTransport},
};
use anyhow::{bail, Result};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};
const HELPER: &str = concat!(
    include_str!("settings_paths.py"),
    "\n",
    include_str!("artifact_read.py")
);
const HEADER: &[u8] = b"fridica-artifact-v1\0";

impl SshTransport {
    pub async fn read_file(
        &self,
        path: PathBuf,
        roots: Vec<PathBuf>,
        limit: usize,
        environment: BTreeMap<OsString, OsString>,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        if limit > ARTIFACT_LIMIT || timeout.is_zero() || timeout > Duration::from_secs(120) {
            bail!("invalid artifact read bounds");
        }
        let launch = self.launch(
            vec![
                "/usr/bin/python3".into(),
                "-I".into(),
                "-S".into(),
                "-c".into(),
                include_str!("isolation_bootstrap.py").into(),
                HELPER.into(),
                serde_json::json!({"path":path,"roots":roots,"limit":limit}).to_string(),
            ],
            "/",
            environment,
            &BTreeMap::new(),
            LaunchOptions::default(),
        )?;
        let result = process::run_with_open_stdin(launch, timeout, limit + 4096).await?;
        if result.returncode != 0 || !result.stdout.starts_with(HEADER) {
            bail!("artifact read failed");
        }
        let body = &result.stdout[HEADER.len()..];
        let Some(length) = body.get(..8) else {
            bail!("incomplete artifact frame");
        };
        let length = u64::from_be_bytes(length.try_into()?);
        let data = &body[8..];
        if length != data.len() as u64 || data.len() > limit {
            bail!("invalid artifact frame length");
        }
        Ok(data.to_vec())
    }
}
