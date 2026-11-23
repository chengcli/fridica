//! Bundled alphaXiv literature tools.
use crate::{
    config::{registry::Policy, Config},
    research::Service,
};

pub fn available(config: &Config, machine: &str, policy: &Policy) -> bool {
    Service::Alphaxiv.available(config, machine, policy)
}
pub fn servers(config: &Config, machine: &str, policy: &Policy) -> Vec<fridica_agent::McpServer> {
    Service::Alphaxiv
        .server(config, machine, policy)
        .into_iter()
        .collect()
}
