//! Native candidate identity and service printing.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub version: String,
    pub source_id: String,
    pub target: String,
}
impl Build {
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").into(),
            source_id: option_env!("FRIDICA_BUILD_ID")
                .unwrap_or("unpackaged")
                .into(),
            target: option_env!("FRIDICA_BUILD_TARGET")
                .unwrap_or(env!("FRIDICA_COMPILE_TARGET"))
                .into(),
        }
    }
}
fn unit_path(path: &Path) -> Result<String> {
    let raw = path.to_str().context("service path must be UTF-8")?;
    if !path.is_absolute() || raw.chars().any(char::is_control) {
        bail!("service paths must be absolute without control characters");
    }
    Ok(format!(
        "\"{}\"",
        raw.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}
pub fn service(
    executable: &Path,
    config: &Path,
    environment: &Path,
    observe_only: bool,
) -> Result<String> {
    let executable = unit_path(executable)?;
    let config = unit_path(config)?;
    // EnvironmentFile supports specifiers but does not perform ExecStart's $ expansion.
    let environment = unit_path(environment)?.replace("$$", "$");
    let mode = if observe_only { " --observe-only" } else { "" };
    Ok(format!("[Unit]\nDescription=Fridica experimental native candidate\nAfter=network-online.target\nStartLimitIntervalSec=600\nStartLimitBurst=3\n\n[Service]\nType=simple\nUMask=0077\nEnvironmentFile={environment}\nExecStart={executable} start --config {config}{mode}\nRestart=on-failure\nRestartSec=5\nKillMode=control-group\nTimeoutStopSec=180\n\n[Install]\nWantedBy=default.target\n"))
}
