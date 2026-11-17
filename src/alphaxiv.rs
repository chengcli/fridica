//! Bundled literature tools, enabled explicitly and constrained by placement policy.
use crate::config::{registry::Policy, Config};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
}
impl Settings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

pub fn available(config: &Config, machine: &str, policy: &Policy) -> bool {
    config.alphaxiv.enabled
        && config
            .machines
            .get(machine)
            .is_some_and(|m| matches!(m.transport.as_str(), "local" | "ssh"))
        && policy.fetch_repos.is_empty()
        && (policy.any_network()
            || policy.network.iter().any(|domain| {
                let domain = domain.to_ascii_lowercase();
                domain == "api.alphaxiv.org"
                    || domain
                        .strip_prefix("*.")
                        .is_some_and(|suffix| "api.alphaxiv.org".ends_with(&format!(".{suffix}")))
            }))
}

pub fn servers(config: &Config, machine: &str, policy: &Policy) -> Vec<fridica_agent::McpServer> {
    if !available(config, machine, policy) {
        return vec![];
    }
    let mut server = fridica_agent::alphaxiv::alphaxiv("fridica");
    server.args = vec!["alphaxiv-mcp".into()];
    if config
        .machines
        .get(machine)
        .is_some_and(|m| m.transport == "local")
    {
        if let Ok(exe) = std::env::current_exe() {
            if exe
                .file_name()
                .is_some_and(|n| n == "fridica" || n == "fridica-candidate")
            {
                server.command = exe.to_string_lossy().into_owned();
            }
        }
    }
    vec![server]
}
