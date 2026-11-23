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
