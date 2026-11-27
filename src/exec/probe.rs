//! Fixed commands run on a machine to look at it: load readings, readiness
//! and diagnostics. Each starts in `/` with no environment beyond the
//! machine's own, locally or over SSH.
use super::{
    isolation::{read_only_ssh_probe, shared_ssh_probe},
    local::LocalTransport,
    process::Launch,
    ssh::{LaunchOptions, SshTransport},
};
use crate::config::registry::Machine;
use anyhow::Result;
use std::{collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt, path::Path};
use tempfile::TempDir;

/// How an SSH probe connects.
pub enum Control<'a> {
    /// On its own: no master connection is reused or left behind and no host
    /// key is enrolled. The transport still wants a private control
    /// directory, so one with this prefix lives as long as the probe.
    ReadOnly(&'a str),
    /// Through the caller's private control directory, shared by its probes.
    Shared(&'a Path),
}

/// The launch for `args` on `machine`, and the directory to keep until the
/// process and its cleanup have exited. A machine that is not local is
/// reached over SSH, which refuses any other transport.
pub fn launch(
    machine: &Machine,
    home: &Path,
    excluded_env: Vec<String>,
    inherited: BTreeMap<OsString, OsString>,
    args: Vec<String>,
    control: Control<'_>,
) -> Result<(Launch, Option<TempDir>)> {
    if machine.transport == "local" {
        let launch = LocalTransport {
            machine: machine.clone(),
            home: home.to_owned(),
            excluded_env,
        }
        .launch(
            args,
            Path::new("/"),
            inherited,
            &BTreeMap::new(),
            None,
            false,
        )?;
        return Ok((launch, None));
    }
    let (directory, held) = match control {
        Control::ReadOnly(prefix) => {
            let held = tempfile::Builder::new()
                .prefix(prefix)
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()?;
            (held.path().to_owned(), Some(held))
        }
        Control::Shared(directory) => (directory.to_owned(), None),
    };
    let mut launch = SshTransport {
        machine: machine.clone(),
        excluded_env,
        control_directory: directory,
    }
    .launch(
        args,
        "/",
        inherited,
        &BTreeMap::new(),
        LaunchOptions::default(),
    )?;
    if held.is_some() {
        read_only_ssh_probe(&mut launch);
    } else {
        shared_ssh_probe(&mut launch);
    }
    Ok((launch, held))
}

/// Retry only a diagnostic SSH handshake that failed before a remote command
/// could start. All attempts share the caller's original time budget.
pub async fn run(
    launch: Launch,
    timeout: std::time::Duration,
) -> Result<super::process::Completed> {
    use std::time::Duration;
    use tokio::time::{sleep, Instant};
    let ssh = launch
        .argv
        .get(super::credentials::ssh_options_index(&launch) - 1)
        .is_some_and(|program| program == "ssh");
    let deadline = Instant::now() + timeout;
    for delay in [Duration::from_secs(2), Duration::from_secs(5)] {
        let result = super::process::run_with_open_stdin(
            launch.clone(),
            deadline.saturating_duration_since(Instant::now()),
            4096,
        )
        .await?;
        if !ssh || !handshake_reset(&result) || Instant::now() + delay >= deadline {
            return Ok(result);
        }
        sleep(delay).await;
    }
    super::process::run_with_open_stdin(
        launch,
        deadline.saturating_duration_since(Instant::now()),
        4096,
    )
    .await
}

pub(crate) fn handshake_reset(result: &super::process::Completed) -> bool {
    result.returncode == 255
        && result.stdout.is_empty()
        && String::from_utf8_lossy(&result.stderr).lines().any(|line| {
            line.starts_with("kex_exchange_identification:")
                && (line.contains("Connection reset") || line.contains("Connection closed"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    #[tokio::test]
    async fn retries_handshake_reset_before_remote_execution() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        std::fs::write(&ssh, "#!/bin/sh\nif [ -e attempted ]; then echo ready; exit 0; fi\ntouch attempted\necho 'kex_exchange_identification: read: Connection reset by peer' >&2\nexit 255\n").unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let launch = Launch {
            argv: vec!["ssh".into()],
            cwd: Some(dir.path().to_owned()),
            env: [(
                OsString::from("PATH"),
                std::env::join_paths([dir.path(), Path::new("/usr/bin")]).unwrap(),
            )]
            .into(),
        };
        let completed = run(launch, Duration::from_secs(10)).await.unwrap();
        assert_eq!(completed.returncode, 0);
        assert_eq!(completed.stdout, b"ready\n");
    }
}
