//! Shared discovery for installed native resources; never downloads or reads credentials.
use crate::config::overleaf::Login;
use anyhow::{ensure, Context, Result};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub(crate) fn node(login: &Login) -> Result<PathBuf> {
    if let Some(p) = &login.node {
        ensure!(executable(p), "configured Node is not executable");
        return Ok(p.clone());
    }
    let mut paths: Vec<_> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join("node"))
        .collect();
    paths.extend(
        [
            "/opt/homebrew/bin/node",
            "/usr/local/bin/node",
            "/usr/bin/node",
        ]
        .map(PathBuf::from),
    );
    paths
        .into_iter()
        .find(|p| executable(p))
        .context("Node >=20 not found; install Node >=20 and make node available on PATH, including the worker's PATH")
}
fn executable(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
pub(crate) fn resources(login: &Login, config: Option<&Path>, home: &Path) -> Result<PathBuf> {
    let mut candidates = vec![];
    if let Some(p) = &login.resources {
        candidates.push(p.clone());
    } else {
        if let Some(config) = config {
            candidates.push(
                config
                    .parent()
                    .context("config directory missing")?
                    .join("overleaf-native"),
            );
        }
        if let Some(prefix) = std::env::current_exe()?.parent().and_then(Path::parent) {
            candidates.push(prefix.join("share/fridica/overleaf-native"));
        }
        candidates.push(home.join("Library/Application Support/Fridica/overleaf-native"));
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        candidates.push(data.join("fridica/overleaf-native"));
    }
    candidates.into_iter().find(|p| p.join("node_modules/@netique/overleaf-mcp/dist/auth/browserLogin.js").is_file())
        .context("installed Overleaf resources missing; install a Fridica wheel or prepare source resources explicitly")?.canonicalize().context("cannot resolve Overleaf resources")
}
