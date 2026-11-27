//! Constrain the stdio server itself. Git and TeX inherit the same kernel policy.
use anyhow::{ensure, Context, Result};
use std::{
    os::unix::process::CommandExt,
    path::{Component, Path},
    process::Command,
};

pub fn launch(workspace: &Path, probe: bool, thread_project: Option<&str>) -> Result<()> {
    let root = workspace_root(workspace)?;
    let mut command = sandbox(&root, None)?;
    command
        .arg(std::env::current_exe()?)
        .arg("overleaf-mcp")
        .env("INTERLEAF_WORKSPACE", &root)
        .env("TMPDIR", &root)
        .current_dir(&root);
    if let Some(project) = thread_project {
        command.args(["--thread-project", project]);
    }
    if probe {
        command.arg("--probe");
    }
    Err(command.exec()).context("could not start the workspace sandbox for Overleaf")
}

/// Give only this MCP process and its children the job's Git destination check.
pub fn serve_project(project: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    ensure!(
        project == "unbound" || crate::config::overleaf::valid_id(project),
        "invalid thread project"
    );
    let path = std::env::var_os("PATH").context("missing executable path")?;
    let dirs: Vec<_> = std::env::split_paths(&path).collect();
    let git = dirs
        .iter()
        .map(|dir| dir.join("git"))
        .find(|p| p.is_absolute() && p.is_file());
    let temp = tempfile::tempdir()?;
    let wrapper = temp.path().join("git");
    std::fs::write(&wrapper, include_str!("overleaf_git.py"))?;
    std::fs::set_permissions(wrapper, std::fs::Permissions::from_mode(0o700))?;
    let path =
        std::env::join_paths(std::iter::once(temp.path()).chain(dirs.iter().map(|d| d.as_path())))?;
    let mut child = Command::new(std::env::current_exe()?);
    if project != "unbound" {
        child.env("OVERLEAF_PROJECT_ID", project);
    }
    // Native review and tool discovery also run on workers without Git.
    child.env_remove("FRIDICA_REAL_GIT");
    if let Some(git) = git {
        child.env("FRIDICA_REAL_GIT", git);
    }
    let status = child
        .arg("overleaf-mcp")
        .env("PATH", path)
        .env("FRIDICA_THREAD_PROJECT", project)
        .status()?;
    ensure!(status.success(), "Overleaf server exited unsuccessfully");
    Ok(())
}

/// The native MCP is bundled Rust. Grant transport remains the scoped launcher environment.
pub fn launch_native(
    workspace: &Path,
    worker: &str,
    channel: &str,
    project: &str,
    session_stdin: bool,
) -> Result<()> {
    if !session_stdin {
        let endpoint = std::env::var(crate::config::overleaf::NATIVE_ENV)
            .context("native endpoint missing; launch through Fridica")?;
        let path = endpoint
            .strip_prefix("unix:")
            .context("native tools require the Fridica session broker")?;
        return super::broker::proxy(Path::new(path));
    }
    let raw = super::read_grant(std::io::stdin().lock())?;
    let grant = crate::config::overleaf::task_session(&raw, worker, channel, project)?;
    let root = workspace_root(workspace)?;
    let state = native_state(&grant.account_id, project, &grant.base_url)?;
    let mut command = sandbox(&root, Some(&state))?;
    command
        .arg(std::env::current_exe()?)
        .arg("overleaf-mcp")
        .args([
            "--native-worker",
            worker,
            "--native-channel",
            channel,
            "--thread-project",
            project,
        ])
        .arg("--native-serve")
        .arg("--native-session-stdin")
        .env_remove(crate::config::overleaf::NATIVE_ENV)
        .env("INTERLEAF_WORKSPACE", &root)
        .env("TMPDIR", &root)
        .current_dir(&root);
    for name in [
        "OVERLEAF_GIT_TOKEN",
        "ADS_TOKEN",
        "OVERLEAF_PROJECT_ID",
        "INTERLEAF_CONFIG",
    ] {
        command.env_remove(name);
    }
    // Forward the grant in memory before MCP input, never in a child environment.
    use std::io::Write;
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::piped())
        .spawn()
        .context("could not start native Overleaf sandbox")?;
    let mut input = child.stdin.take().context("native sandbox input missing")?;
    input.write_all(raw.as_bytes())?;
    input.write_all(b"\n")?;
    input.flush()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut input);
    });
    let status = child.wait()?;
    ensure!(
        status.success(),
        "native Overleaf sandbox exited unsuccessfully"
    );
    Ok(())
}

pub fn probe_native(workspace: &Path) -> Result<()> {
    let root = workspace_root(workspace)?;
    let mut command = sandbox(&root, None)?;
    command
        .arg(std::env::current_exe()?)
        .arg("overleaf-mcp")
        .arg("--native-discovery")
        .env("TMPDIR", &root)
        .current_dir(&root);
    for name in [
        crate::config::overleaf::NATIVE_ENV,
        "OVERLEAF_GIT_TOKEN",
        "ADS_TOKEN",
    ] {
        command.env_remove(name);
    }
    Err(command.exec()).context("could not probe bundled Rust Overleaf tools")
}

pub fn discover_native() -> Result<()> {
    ensure!(
        fridica_agent::overleaf::probe_native()?
            == fridica_agent::overleaf::native_operations().len() + 2,
        "native tools unavailable"
    );
    println!("fridica-overleaf-mcp:native-ready");
    Ok(())
}

pub fn serve_native(worker: &str, channel: &str, project: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let raw = super::read_grant(std::io::stdin().lock())?;
    let grant = crate::config::overleaf::task_session(&raw, worker, channel, project)?;
    let state = native_state(&grant.account_id, project, &grant.base_url)?;
    let workspace = std::env::current_dir()?.canonicalize()?;
    let mut legacy = grant.legacy_workspaces;
    legacy.push(workspace.clone());
    let legacy = legacy_receipt_dirs(&legacy)?;
    let session = fridica_agent::overleaf::NativeSession::from_reader(
        serde_json::json!({"baseUrl":grant.base_url,"cookie":grant.cookie})
            .to_string()
            .as_bytes(),
        state,
    )?
    .with_identity(&grant.account_id, &grant.expected_email)?
    .with_capabilities(grant.projects, grant.operations)?
    .with_upload_roots(vec![workspace])?;
    fridica_agent::overleaf::run_native(session, |name, args| {
        crate::config::overleaf::permit_tool(&raw, worker, channel, project, name, args)?;
        // Older adapter receipts are authoritative too. Never replay one during migration.
        if let Some(operation) = args.get("operation_id").and_then(|v| v.as_str()) {
            let filename = format!("{:x}.json", Sha256::digest(operation.as_bytes()));
            for directory in &legacy {
                ensure!(
                    !directory.join(&filename).try_exists()?,
                    "legacy operation receipt exists; reconcile it read-only before continuing"
                );
            }
        }
        Ok(())
    })
}

fn legacy_receipt_dirs(workspaces: &[std::path::PathBuf]) -> Result<Vec<std::path::PathBuf>> {
    let mut result = std::collections::BTreeSet::new();
    for workspace in workspaces {
        let root = workspace_root(workspace)?;
        result.insert(root.join(".fridica-overleaf-receipts"));
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context("cannot inspect legacy receipt workspace"),
        };
        for (index, entry) in entries.enumerate() {
            ensure!(index < 16384, "legacy workspace entry limit exceeded");
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_prefix("worker"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            {
                ensure!(
                    !entry.file_type()?.is_symlink(),
                    "legacy worker directory must not be a symlink"
                );
                if entry.file_type()?.is_dir() {
                    result.insert(entry.path().join(".fridica-overleaf-receipts"));
                }
            }
        }
    }
    Ok(result.into_iter().collect())
}

pub(super) fn workspace_root(workspace: &Path) -> Result<std::path::PathBuf> {
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
    Ok(root)
}

/// A project journal survives worker/slot changes on the same execution machine.
fn native_state(account: &str, project: &str, origin: &str) -> Result<std::path::PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    ensure!(
        crate::config::overleaf::valid_id(account)
            && (project == "unbound" || crate::config::overleaf::valid_id(project)),
        "invalid native state identity"
    );
    let home = std::env::home_dir()
        .context("missing worker home")?
        .canonicalize()?;
    let mut path = home;
    let origin_key = if origin == crate::config::overleaf::DEFAULT_ORIGIN {
        account.to_owned()
    } else {
        use sha2::{Digest, Sha256};
        format!("{account}-{:x}", Sha256::digest(origin.as_bytes()))
    };
    for component in [
        ".local",
        "state",
        "fridica",
        "overleaf",
        &origin_key,
        project,
    ] {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "native state directory must not be a symlink"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::DirBuilder::new().mode(0o700).create(&path)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let mode = std::fs::metadata(&path)?.permissions().mode();
    ensure!(
        mode & 0o077 == 0,
        "native project state must be private (0700)"
    );
    Ok(path)
}

#[cfg(target_os = "linux")]
fn sandbox(root: &Path, state: Option<&Path>) -> Result<Command> {
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
        .arg(root);
    if let Some(state) = state {
        let fd = openat2(
            CWD,
            state,
            OFlags::PATH | OFlags::DIRECTORY,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .context("cannot pin native state")?;
        command
            .arg("--bind-fd")
            .arg(fd.into_raw_fd().to_string())
            .arg(state);
    }
    command.arg("--chdir").arg(root).arg("--");
    Ok(command)
}

#[cfg(target_os = "macos")]
fn sandbox(root: &Path, state: Option<&Path>) -> Result<Command> {
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
        .arg("-D")
        .arg(format!("STATE={}", state.unwrap_or(root).display()))
        .args(["-p", include_str!("overleaf.sb")]);
    Ok(command)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sandbox(_: &Path, _: Option<&Path>) -> Result<Command> {
    anyhow::bail!("Overleaf workspace confinement requires Linux or macOS")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_receipts_remain_visible_after_slot_change() {
        let root = tempfile::tempdir().unwrap();
        let prior = root.path().join("worker9/.fridica-overleaf-receipts");
        std::fs::create_dir_all(&prior).unwrap();
        let current = root.path().join("worker2");
        std::fs::create_dir(&current).unwrap();
        let dirs = legacy_receipt_dirs(&[root.path().to_owned(), current]).unwrap();
        assert!(dirs.contains(&prior));
        let name = "retained-operation.json";
        std::fs::write(prior.join(name), b"{}").unwrap();
        assert!(dirs.iter().any(|dir| dir.join(name).try_exists().unwrap()));
    }
}
