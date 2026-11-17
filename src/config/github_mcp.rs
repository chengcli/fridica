//! Host-owned, per-machine GitHub MCP settings. No credentials or I/O.
use super::Config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub machines: BTreeMap<String, Target>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            machines: BTreeMap::new(),
        }
    }
}
impl Settings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Target {
    pub command: String,
    pub token_env: String,
}
impl Default for Target {
    fn default() -> Self {
        Self {
            // Empty selects the bundled `fridica github-mcp` subcommand.
            command: String::new(),
            token_env: "GH_TOKEN".into(),
        }
    }
}
pub fn validate(config: &Config) -> anyhow::Result<()> {
    for (machine, target) in &config.github.mcp.machines {
        anyhow::ensure!(
            config.machines.get(machine).is_some(),
            "github.mcp names an unknown machine"
        );
        anyhow::ensure!(
            !target.command.chars().any(char::is_control)
                && (!target.command.contains('/')
                    || target.command.starts_with('/')
                    || target.command.starts_with("~/")),
            "github.mcp command must be an executable name or path"
        );
        let name = &target.token_env;
        anyhow::ensure!(
            !name.is_empty()
                && !name.as_bytes()[0].is_ascii_digit()
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !name.to_uppercase().contains("SLACK")
                && !name.to_uppercase().starts_with("FRIDICA_")
                && !config.secret_env().contains(&name.as_str()),
            "github.mcp token_env must name a worker credential, not a daemon credential"
        );
    }
    Ok(())
}
