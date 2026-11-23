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

/// Verify read access to the default Overleaf project using Git's read-only
/// reference advertisement. A project ID is needed to authenticate the token.
pub async fn probe_overleaf() -> anyhow::Result<bool> {
    fridica_agent::overleaf::probe().await?;
    let Some(project) = std::env::var("OVERLEAF_PROJECT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
    else {
        return Ok(false);
    };
    anyhow::ensure!(
        project.len() == 24 && project.bytes().all(|c| c.is_ascii_hexdigit()),
        "OVERLEAF_PROJECT_ID must be a 24-digit hexadecimal project ID"
    );
    let url = format!("https://git.overleaf.com/{project}/info/refs?service=git-upload-pack");
    verify_git_access(&url, &std::env::var("OVERLEAF_GIT_TOKEN")?).await?;
    Ok(true)
}

async fn verify_git_access(url: &str, token: &str) -> anyhow::Result<()> {
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()?
        .get(url)
        .basic_auth("git", Some(token))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Overleaf project check failed"))?;
    anyhow::ensure!(
        response.status().is_success()
            && response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(
                    |v| v.split(';').next() == Some("application/x-git-upload-pack-advertisement")
                ),
        "Overleaf project read access could not be verified"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn overleaf_verifies_an_authenticated_git_read() {
        crate::exec::tls::initialize();
        for (status, content_type, expected) in [
            (
                "200 OK",
                "application/x-git-upload-pack-advertisement",
                true,
            ),
            ("200 OK", "text/html", false),
            ("401 Unauthorized", "text/plain", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!(
                "http://{}/project/info/refs?service=git-upload-pack",
                listener.local_addr().unwrap()
            );
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(
                    line,
                    "GET /project/info/refs?service=git-upload-pack HTTP/1.1\r\n"
                );
                let mut authenticated = false;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    authenticated |=
                        line.eq_ignore_ascii_case("authorization: Basic Z2l0OnRlc3Q=\r\n");
                }
                assert!(authenticated);
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: 0\r\n\r\n");
                reader
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            });
            assert_eq!(verify_git_access(&url, "test").await.is_ok(), expected);
            server.await.unwrap();
        }
    }
}
