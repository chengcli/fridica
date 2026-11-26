//! Constrain the stdio server itself. Git and TeX inherit the same kernel policy.
use anyhow::{ensure, Context, Result};
use std::{
    os::unix::process::CommandExt,
    path::{Component, Path},
    process::Command,
};

pub fn launch(workspace: &Path, probe: bool) -> Result<()> {
    let root = match workspace.strip_prefix("~") {
        Ok(rest) => std::env::home_dir()
            .context("missing worker home")?
            .join(rest),
        Err(_) => workspace.to_owned(),
    };
    ensure!(
        root.is_absolute()
            && root.parent().is_some()
            && !root.components().any(|c| c == Component::ParentDir),
        "expected an absolute worker workspace"
    );
    let mut command = sandbox(&root)?;
    command
        .arg(std::env::current_exe()?)
        .arg("overleaf-mcp")
        .env("INTERLEAF_WORKSPACE", &root)
        .env("TMPDIR", &root)
        .current_dir(&root);
    if probe {
        command.arg("--probe");
    }
    Err(command.exec()).context("could not start the workspace sandbox for Overleaf")
}

/// Installed-wheel worker entry. Resolve the companion package on this machine,
/// not on the owner desktop; the adapter retains the built-in Git/compile server.
pub fn launch_native(workspace: &Path, worker: &str, channel: &str) -> Result<()> {
    Err(native_command(workspace)?
        .arg("--worker")
        .arg(worker)
        .arg("--channel")
        .arg(channel)
        .exec())
    .context("could not start installed native Overleaf adapter")
}

pub fn probe_native(workspace: &Path) -> Result<()> {
    Err(native_command(workspace)?.arg("--probe").exec())
        .context("could not probe installed native Overleaf resources")
}

fn native_command(workspace: &Path) -> Result<Command> {
    let home = std::env::home_dir().context("missing worker home")?;
    let login = crate::config::overleaf::Login::default();
    let resources = crate::research::overleaf::resources(&login, None, &home)?;
    let node = crate::research::overleaf::node(&login)?;
    let adapter = resources.join("adapter.py");
    ensure!(adapter.is_file(), "installed Overleaf adapter missing");
    let root = workspace
        .strip_prefix("~")
        .map_or_else(|_| workspace.to_owned(), |rest| home.join(rest));
    ensure!(
        root.is_absolute()
            && root.parent().is_some()
            && !root.components().any(|c| c == Component::ParentDir),
        "expected an absolute worker workspace"
    );
    // Worker launch/probes already activate the execution target's configured
    // Python environment; MCP subprocesses must inherit that PATH selection.
    let mut command = Command::new("python3");
    command
        .arg("-B")
        .arg(adapter)
        .arg("--node")
        .arg(node)
        .arg("--builtin")
        .arg(std::env::current_exe()?)
        .arg("--workspace")
        .arg(root);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn sandbox(root: &Path) -> Result<Command> {
    use rustix::fs::{openat2, Mode, OFlags, ResolveFlags, CWD};
    use std::os::fd::IntoRawFd;
    // Pin the exact directory across exec. A slot symlink must never select a
    // different writable tree. bwrap consumes and closes the mount descriptor.
    let fd = openat2(
        CWD,
        root,
        OFlags::PATH | OFlags::DIRECTORY,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )
    .context("cannot pin Overleaf workspace")?;
    let mut command = Command::new("/usr/bin/bwrap");
    command
        .args([
            "--die-with-parent",
            "--new-session",
            "--unshare-all",
            "--share-net",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--bind-fd",
        ])
        .arg(fd.into_raw_fd().to_string())
        .arg(root)
        .arg("--chdir")
        .arg(root)
        .arg("--");
    Ok(command)
}

#[cfg(target_os = "macos")]
fn sandbox(root: &Path) -> Result<Command> {
    let canonical = root
        .canonicalize()
        .context("cannot resolve Overleaf workspace")?;
    ensure!(
        canonical == root && canonical.is_dir(),
        "expected a resolved Overleaf workspace directory"
    );
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .arg("-D")
        .arg(format!("WORKSPACE={}", root.display()))
        .args(["-p", include_str!("overleaf.sb")]);
    Ok(command)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sandbox(_: &Path) -> Result<Command> {
    anyhow::bail!("Overleaf workspace confinement requires Linux or macOS")
}
