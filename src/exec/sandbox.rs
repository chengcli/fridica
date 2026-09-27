//! Existing bubblewrap layout. This is GPU/filesystem confinement, not a
//! credential sandbox: backend state remains writable and host network shared.
use super::shell;
use anyhow::{bail, Result};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
};
const STATE: [&str; 3] = [".codex", ".claude", ".claude.json"];
const SETTINGS: [&str; 4] = [
    ".codex/config.toml",
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".claude/hooks",
];
const DEFAULTS: [(&str, &str); 3] = [
    (".claude/settings.json", "{}"),
    (".claude/settings.local.json", "{}"),
    (".codex/config.toml", ""),
];
pub fn confinement(roots: &[String], home: Option<&str>) -> Result<Vec<String>> {
    let mut words: Vec<String> = [
        "bwrap",
        "--die-with-parent",
        "--unshare-user",
        "--unshare-pid",
        "--ro-bind",
        "/",
        "/",
        "--dev-bind",
        "/dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
    ]
    .map(str::to_owned)
    .into();
    for root in roots {
        if root.contains('\0') || !(root.starts_with('/') || root.starts_with("~/")) {
            bail!("invalid confinement root");
        }
        let path = match root.strip_prefix("~/") {
            Some(tail) => format!("{}/{tail}", home.unwrap_or("$HOME")),
            None => root.clone(),
        };
        words.extend(["--bind".into(), path.clone(), path]);
    }
    for (flag, names) in [("--bind-try", &STATE[..]), ("--ro-bind-try", &SETTINGS[..])] {
        for name in names {
            let path = format!("{}/{name}", home.unwrap_or("$HOME"));
            words.extend([flag.into(), path.clone(), path]);
        }
    }
    words.push("--".into());
    Ok(words)
}
pub fn shell_words(words: &[String]) -> String {
    words
        .iter()
        .map(|w| match w.strip_prefix("$HOME/") {
            Some(tail) => format!("\"$HOME/\"{}", shell::quote(tail)),
            None => shell::quote(w),
        })
        .collect::<Vec<_>>()
        .join(" ")
}
pub fn prepare_local(home: &Path) -> Result<()> {
    for directory in [".claude/hooks", ".codex"] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(home.join(directory))?;
    }
    for (name, body) in DEFAULTS {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(home.join(name))
        {
            Ok(mut file) => file.write_all(body.as_bytes())?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub fn prepare_script() -> String {
    let mut lines = vec!["mkdir -p \"$HOME/.claude/hooks\" \"$HOME/.codex\"".into()];
    for (name, body) in DEFAULTS {
        // noclobber prevents a racing creator from having its settings replaced.
        lines.push(format!(
            "[ -e \"$HOME/{name}\" ] || (umask 077; set -C; printf %s {} > \"$HOME/{name}\")",
            shell::quote(body)
        ));
    }
    lines.join("; ")
}
