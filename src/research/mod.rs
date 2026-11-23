//! Bundled research tools and the worker placements that can run them.
use crate::config::{registry::Policy, Config};
use fridica_agent::McpServer;

#[derive(Clone, Copy)]
pub enum Service {
    Alphaxiv,
    Scix,
    Overleaf,
}
pub const SERVICES: [Service; 3] = [Service::Alphaxiv, Service::Scix, Service::Overleaf];

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Self::Alphaxiv => "alphaxiv",
            Self::Scix => "scix",
            Self::Overleaf => "overleaf",
        }
    }
    pub fn enabled(self, config: &Config) -> bool {
        match self {
            Self::Alphaxiv => config.alphaxiv.enabled,
            Self::Scix => config.scix.enabled,
            Self::Overleaf => config.overleaf.enabled,
        }
    }
    pub fn available(self, config: &Config, machine: &str, policy: &Policy) -> bool {
        self.enabled(config)
            && config
                .machines
                .get(machine)
                .is_some_and(|m| matches!(m.transport.as_str(), "local" | "ssh"))
            && policy.fetch_repos.is_empty()
            && match self {
                Self::Alphaxiv => allows_host(policy, "api.alphaxiv.org"),
                // ADS also exposes tools that modify the owner's libraries.
                Self::Scix => {
                    policy.mode != "read-only" && allows_host(policy, "api.adsabs.harvard.edu")
                }
                // Interleaf accepts arbitrary Git remotes and local checkout paths.
                Self::Overleaf => policy.mode == "full" && policy.any_network(),
            }
    }
    pub fn instructions(self) -> &'static str {
        match self {
            Self::Alphaxiv => fridica_agent::alphaxiv::INSTRUCTIONS,
            Self::Scix => fridica_agent::scix::INSTRUCTIONS,
            Self::Overleaf => "Use the Overleaf tools for the requested project. Preview changes before pushing and report whether the push succeeded.",
        }
    }
    pub fn server(self, config: &Config, machine: &str, policy: &Policy) -> Option<McpServer> {
        if !self.available(config, machine, policy) {
            return None;
        }
        let mut server = match self {
            Self::Alphaxiv => fridica_agent::alphaxiv::alphaxiv("fridica"),
            Self::Scix => fridica_agent::scix::scix("fridica"),
            Self::Overleaf => fridica_agent::overleaf::overleaf("fridica"),
        };
        server.args = vec![format!("{}-mcp", self.name())];
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
        Some(server)
    }
}

fn allows_host(policy: &Policy, host: &str) -> bool {
    policy.any_network()
        || policy.network.iter().any(|domain| {
            let domain = domain.to_ascii_lowercase();
            domain == host
                || domain
                    .strip_prefix("*.")
                    .is_some_and(|suffix| host.ends_with(&format!(".{suffix}")))
        })
}
