//! SSH argv and remote lifecycle templates. Uses existing owner SSH configuration;
//! never enables agent forwarding or accepts arbitrary transport shell fragments.
use super::{process, shell};
use crate::config::registry::ssh_host;
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

pub fn control_directory(runtime: Option<&Path>, uid: u32) -> Result<PathBuf> {
    let directory = runtime
        .filter(|p| p.is_dir())
        .map(|p| p.join("fridica"))
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/fridica-ssh-{uid}")));
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let info = fs::symlink_metadata(&directory)?;
    if !info.is_dir() || info.uid() != uid || info.mode() & 0o077 != 0 {
        bail!("SSH control directory must be private and owned by this user");
    }
    if directory.as_os_str().len() + 3 >= 100 {
        bail!("SSH control socket path is too long");
    }
    Ok(directory)
}
pub fn command(host: &str, script: &str, directory: &Path) -> Result<Vec<String>> {
    if !ssh_host(host) || script.contains('\0') {
        bail!("invalid SSH destination or script");
    }
    let control = directory
        .join("%C")
        .to_str()
        .context("SSH control path is not UTF-8")?
        .to_owned();
    Ok(vec![
        "ssh".into(),
        "-T".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=15".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ServerAliveCountMax=4".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={control}"),
        "-o".into(),
        "ControlPersist=600".into(),
        "--".into(),
        host.into(),
        script.into(),
    ])
}
/// Long-lived channels use a private FIFO and terminate their process group when
/// input closes. Without setsid (macOS) the wrapper can signal only the agent PID,
/// matching the existing transport's limitation.
fn watchdog(agent: &str) -> Vec<String> {
    vec![
        "umask 077".into(),
        "FRIDICA_DIR=$(mktemp -d \"${TMPDIR:-/tmp}/fridica.XXXXXXXX\") || exit 97".into(),
        "FRIDICA_IN=\"$FRIDICA_DIR/in\"; FRIDICA_AGENT=; FRIDICA_FEED=".into(),
        "fridica_stop() { if [ -n \"$FRIDICA_AGENT\" ]; then kill -TERM -\"$FRIDICA_AGENT\" 2>/dev/null || kill -TERM \"$FRIDICA_AGENT\" 2>/dev/null || :; sleep 1; kill -KILL -\"$FRIDICA_AGENT\" 2>/dev/null || kill -KILL \"$FRIDICA_AGENT\" 2>/dev/null || :; fi; }".into(),
        "fridica_clean() { if [ -n \"$FRIDICA_FEED\" ]; then kill \"$FRIDICA_FEED\" 2>/dev/null || :; wait \"$FRIDICA_FEED\" 2>/dev/null || :; fi; rm -f \"$FRIDICA_IN\"; rmdir \"$FRIDICA_DIR\"; }".into(),
        "trap 'fridica_clean' EXIT".into(),
        "trap 'fridica_stop; exit 143' TERM INT HUP".into(),
        "mkfifo -m 600 \"$FRIDICA_IN\" || exit 97".into(),
        format!("if command -v setsid >/dev/null 2>&1; then setsid {agent} < \"$FRIDICA_IN\" & else {agent} < \"$FRIDICA_IN\" & fi; FRIDICA_AGENT=$!"),
        "exec 3<&0; cat <&3 > \"$FRIDICA_IN\" & FRIDICA_FEED=$!; exec 3<&-".into(),
        "while kill -0 \"$FRIDICA_AGENT\" 2>/dev/null && kill -0 \"$FRIDICA_FEED\" 2>/dev/null; do sleep 1; done".into(),
        "if kill -0 \"$FRIDICA_AGENT\" 2>/dev/null; then fridica_stop; fi".into(),
        "wait \"$FRIDICA_AGENT\"; FRIDICA_STATUS=$?".into(),
        // Also remove tools surviving a normally exited agent without replacing
        // its exit status with the cleanup command's status.
        "kill -KILL -\"$FRIDICA_AGENT\" 2>/dev/null || :".into(),
        "exit \"$FRIDICA_STATUS\"".into(),
    ]
}
pub fn remote_script(
    command: &[String],
    cwd: &str,
    env: &BTreeMap<String, String>,
    timeout: Option<Duration>,
    create: bool,
) -> Result<String> {
    remote_script_with_confinement(command, cwd, env, timeout, create, None)
}
pub(crate) fn remote_script_with_confinement(
    command: &[String],
    cwd: &str,
    env: &BTreeMap<String, String>,
    timeout: Option<Duration>,
    create: bool,
    roots: Option<&[String]>,
) -> Result<String> {
    shell::validate(command)?;
    if cwd.contains('\0')
        || cwd.is_empty()
        || env
            .iter()
            .any(|(k, v)| !shell::env_name(k) || v.contains('\0'))
    {
        bail!("invalid remote path or environment");
    }
    let mut lines = vec!["set -u".into()];
    for (name, value) in env {
        lines.push(format!("export {name}={}", shell::quote(value)));
    }
    let mut argv = vec![];
    if let Some(roots) = roots {
        lines.push(super::sandbox::prepare_script());
        argv = super::sandbox::confinement(roots, None)?;
    }
    if create {
        lines.push(format!("mkdir -p -- {} || exit 98", shell::path(cwd)));
    }
    lines.push(format!("cd {} || exit 98", shell::path(cwd)));
    let prefix = super::sandbox::shell_words(&argv);
    let agent = if prefix.is_empty() {
        shell::join(command)
    } else {
        format!("{prefix} {}", shell::join(command))
    };
    if let Some(timeout) = timeout {
        let seconds = timeout
            .as_secs()
            .checked_add(5)
            .context("remote deadline too large")?
            .max(1);
        // Preserve the existing short-command template. The durable executor
        // must supply a stricter adapter if the remote host lacks timeout.
        lines.push(format!("if command -v timeout >/dev/null 2>&1; then exec timeout -k 5 {seconds} {agent}; else exec {agent}; fi"));
    } else {
        lines.extend(watchdog(&agent));
    }
    Ok(format!("exec sh -c {}", shell::quote(&lines.join("; "))))
}
pub fn failure(host: &str, status: i32, detail: &[u8], secrets: &[String]) -> String {
    let detail = process::diagnostic(detail, process::DIAGNOSTIC_LIMIT, secrets);
    let message = match status {
        255 => format!(
            "could not reach {host} over SSH; check that ssh {host} connects without a prompt"
        ),
        98 => format!("the workspace directory does not exist on {host}"),
        _ => format!(
            "agent on {host} exited with status {status}; check its sign-in and sandbox support"
        ),
    };
    if detail.is_empty() {
        message
    } else {
        format!("{message} ({detail})")
    }
}

pub struct SshTransport {
    pub machine: crate::config::registry::Machine,
    pub excluded_env: Vec<String>,
    pub control_directory: PathBuf,
}
#[derive(Default)]
pub struct LaunchOptions<'a> {
    pub timeout: Option<Duration>,
    pub confine: Option<&'a [String]>,
    pub create: bool,
}
impl SshTransport {
    pub fn launch(
        &self,
        command_words: Vec<String>,
        cwd: &str,
        inherited: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
        extra: &BTreeMap<String, String>,
        options: LaunchOptions<'_>,
    ) -> Result<process::Launch> {
        if self.machine.transport != "ssh" {
            bail!("SSH transport requires an SSH machine");
        }
        let info = fs::symlink_metadata(&self.control_directory)?;
        if !info.is_dir() || info.uid() != users::get_current_uid() || info.mode() & 0o077 != 0 {
            bail!("SSH control directory must be private and owned by this user");
        }
        let mut extra_env = self.machine.resources.environment();
        extra_env.extend(extra.clone());
        let remote_env = process::scrubbed_environment([], &self.excluded_env, &extra_env)
            .into_iter()
            .map(|(k, v)| {
                Ok((
                    k.into_string()
                        .map_err(|_| anyhow::anyhow!("non-UTF8 remote environment name"))?,
                    v.into_string()
                        .map_err(|_| anyhow::anyhow!("non-UTF8 remote environment value"))?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let script = remote_script_with_confinement(
            &command_words,
            cwd,
            &remote_env,
            options.timeout,
            options.create,
            options.confine,
        )?;
        Ok(process::Launch {
            argv: command(&self.machine.host, &script, &self.control_directory)?,
            cwd: None,
            env: process::scrubbed_environment(inherited, &self.excluded_env, &BTreeMap::new()),
        })
    }
}
