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
