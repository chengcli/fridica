//! Native candidate identity, owner deployment attestation and service printing.
use crate::config::Config;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, os::unix::fs::MetadataExt, path::Path};

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
                .unwrap_or("unpackaged")
                .into(),
        }
    }
    fn packaged(&self) -> bool {
        self.source_id.len() == 64
            && self.source_id.bytes().all(|b| b.is_ascii_hexdigit())
            && self.target != "unpackaged"
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub build: Build,
    pub executable_sha256: String,
    pub config_fingerprint: String,
    pub owner: String,
    pub workspace: String,
    pub host: String,
    pub validated_at: f64,
    pub target_conformance: bool,
    pub recovery_rehearsal: bool,
    pub observe_only_reconciliation: bool,
}
fn executable_sha256() -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(fs::read(std::env::current_exe()?)?)
    ))
}
fn hostname() -> Result<String> {
    // Host identity is supplied by the OS, never an environment variable.
    Ok(rustix::system::uname().nodename().to_str()?.to_owned())
}
pub fn attest(config: &Config, build: Build, now: f64) -> Result<Deployment> {
    if !build.packaged() || !now.is_finite() || now <= 0. {
        bail!("deployment records require a packaged candidate and valid time");
    }
    Ok(Deployment {
        build,
        executable_sha256: executable_sha256()?,
        config_fingerprint: config.fingerprint.clone(),
        owner: config.owner.slack_user.clone(),
        workspace: config.slack.workspace.clone(),
        host: hostname()?,
        validated_at: now,
        target_conformance: true,
        recovery_rehearsal: true,
        observe_only_reconciliation: true,
    })
}
pub fn validate(config: &Config, build: &Build, path: &Path) -> Result<()> {
    let file = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .context("deployment record is unavailable")?;
    let file = fs::File::from(file);
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != users::get_current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > 16384
    {
        bail!("deployment record must be a private regular file owned by this user");
    }
    let path = fs::canonicalize(path)?;
    if !config.isolation.private_files.contains(&path) {
        bail!("deployment record must be listed in isolation.private_files");
    }
    let mut data = Vec::new();
    file.take(16385).read_to_end(&mut data)?;
    let record: Deployment = serde_json::from_slice(&data).context("invalid deployment record")?;
    if !build.packaged()
        || record.build != *build
        || record.executable_sha256 != executable_sha256()?
        || record.config_fingerprint != config.fingerprint
        || record.owner != config.owner.slack_user
        || record.workspace != config.slack.workspace
        || record.host != hostname()?
        || !record.validated_at.is_finite()
        || record.validated_at <= 0.
        || !record.target_conformance
        || !record.recovery_rehearsal
        || !record.observe_only_reconciliation
    {
        bail!("deployment validation does not match this build, host and configuration");
    }
    Ok(())
}
pub fn write_attestation(config: &Config, path: &Path, build: Build, now: f64) -> Result<()> {
    if !path.is_absolute() || !config.isolation.private_files.contains(&path.to_owned()) {
        bail!("use an absolute deployment record path listed in isolation.private_files");
    }
    let record = attest(config, build, now)?;
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(
        path.parent().context("deployment record parent missing")?,
    )?;
    file.write_all(&serde_json::to_vec_pretty(&record)?)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)?;
    crate::config::editor::sync_directory(path)
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
    deployment: Option<&Path>,
) -> Result<String> {
    let executable = unit_path(executable)?;
    let config = unit_path(config)?;
    // EnvironmentFile supports specifiers but does not perform ExecStart's $ expansion.
    let environment = unit_path(environment)?.replace("$$", "$");
    let mode = match deployment {
        Some(path) => format!("--active --deployment-record {}", unit_path(path)?),
        None => "--observe-only".into(),
    };
    Ok(format!("[Unit]\nDescription=Fridica experimental native candidate\nAfter=network-online.target\nStartLimitIntervalSec=600\nStartLimitBurst=3\n\n[Service]\nType=simple\nUMask=0077\nEnvironmentFile={environment}\nExecStart={executable} start --config {config} {mode}\nRestart=on-failure\nRestartSec=5\nKillMode=control-group\nTimeoutStopSec=180\n\n[Install]\nWantedBy=default.target\n"))
}
