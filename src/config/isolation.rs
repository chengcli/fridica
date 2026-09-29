//! Owner-provisioned worker boundaries. No credentials, discovery I/O or jobs.
use super::registry::{ssh_host, Registry};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub private_files: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub settings_files: Vec<PathBuf>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub mcp_inventory_complete: bool,
    pub mcp_aliases: Vec<String>,
    pub mcp_urls: Vec<String>,
    pub remote: BTreeMap<String, Remote>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub host: String,
    #[serde(default)]
    pub private_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings_files: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mcp_inventory_complete: bool,
}

impl Settings {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
    pub fn validate(&self, machines: &Registry) -> Result<()> {
        validate_identities(&self.mcp_aliases, &self.mcp_urls)?;
        if self.private_files.len() > 256 || self.private_files.iter().any(|p| !private_path(p)) {
            bail!(
                "isolation.private_files requires absolute file paths below dedicated directories"
            );
        }
        validate_settings_files(
            &self
                .settings_files
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            false,
        )?;
        for (name, inventory) in &self.remote {
            let Some(machine) = machines.get(name) else {
                bail!("isolation.remote refers to an unknown machine");
            };
            if machine.transport != "ssh"
                || !ssh_host(&inventory.host)
                || machine.host != inventory.host
            {
                bail!("isolation.remote host must match its configured SSH machine");
            }
            if inventory.private_files.is_empty()
                && inventory.settings_files.is_empty()
                && !inventory.mcp_inventory_complete
            {
                bail!("remote isolation requires a private-file or MCP source inventory");
            }
            if !inventory.private_files.is_empty() {
                validate_remote_files(&inventory.private_files)?;
            }
            validate_settings_files(&inventory.settings_files, true)?;
        }
        Ok(())
    }
    /// Configuration coverage only. This does not claim target reachability,
    /// inventory completeness or runtime isolation support.
    pub fn summary(&self, machines: &Registry) -> Value {
        let targets: Vec<_> = machines.machines.iter()
            .filter(|m| m.transport == "ssh")
            .map(|m| {
                let required = m.workspaces.iter().any(|w| w.policy.gpu_confine == Some(true));
                let inventory = self.remote.get(&m.name);
                json!({"machine":m.name, "inventory":if inventory.is_some_and(|i| !i.private_files.is_empty()){"configured"}else if required{"missing"}else{"not_required"},
                    "private_file_count":inventory.map_or(0, |v| v.private_files.len()), "settings_file_count":inventory.map_or(0, |v| v.settings_files.len()), "mcp_inventory_complete":inventory.is_some_and(|v| v.mcp_inventory_complete)})
            }).collect();
        json!({"additional_private_file_count":self.private_files.len(),
            "settings_file_count":self.settings_files.len(),"mcp_inventory_complete":self.mcp_inventory_complete,
            "mcp_identity_count":self.mcp_aliases.len()+self.mcp_urls.len(),
            "remote":targets,"runtime_checks":"not_run"})
    }
}

pub fn validate_identities(aliases: &[String], urls: &[String]) -> Result<()> {
    if aliases.len() + urls.len() > 128
        || aliases
            .iter()
            .chain(urls)
            .any(|s| s.is_empty() || s.len() > 1024 || s.chars().any(char::is_control))
        || urls.iter().any(|s| {
            (!s.starts_with("http://") && !s.starts_with("https://"))
                || reqwest::Url::parse(s).map_or(true, |url| {
                    !["http", "https"].contains(&url.scheme())
                        || url.host_str().is_none()
                        || !url.username().is_empty()
                        || url.password().is_some()
                        || url.query().is_some()
                        || url.fragment().is_some()
                        || s.contains(['@', '?', '#'])
                })
        })
    {
        bail!("invalid worker MCP identities; endpoints must not contain credentials, queries or fragments");
    }
    Ok(())
}

pub fn validate_remote_files(files: &[String]) -> Result<()> {
    if files.is_empty()
        || files.len() > 256
        || files
            .iter()
            .any(|p| !remote_path(p) || Path::new(p).parent() == Some(Path::new("/")))
    {
        bail!("remote isolation requires private file paths below dedicated directories");
    }
    Ok(())
}

pub fn remote_path(value: &str) -> bool {
    let expanded = value
        .strip_prefix("~/")
        .map(|tail| format!("/remote-home/{tail}"));
    let path = Path::new(expanded.as_deref().unwrap_or(value));
    path.is_absolute()
        && path != Path::new("/")
        && !value.ends_with('/')
        && value.len() <= 4096
        && !value.chars().any(char::is_control)
        && !value.split('/').any(|part| part == "." || part == "..")
}

fn private_path(path: &Path) -> bool {
    path.is_absolute()
        && path.parent().is_some_and(|p| p != Path::new("/"))
        && path
            .to_str()
            .is_some_and(|s| s.len() <= 4096 && !s.chars().any(char::is_control))
        && path
            .components()
            .all(|p| matches!(p, Component::RootDir | Component::Normal(_)))
}

pub fn validate_settings_files(files: &[String], remote: bool) -> Result<()> {
    if files.len() > 64
        || files.iter().any(|p| {
            !remote_path(p)
                || (!remote && !Path::new(p).is_absolute())
                || !matches!(
                    Path::new(p).extension().and_then(|s| s.to_str()),
                    Some("toml" | "json")
                )
        })
    {
        bail!("MCP settings files require bounded absolute or target-home TOML/JSON paths");
    }
    Ok(())
}
