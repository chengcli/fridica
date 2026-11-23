//! Native credential access for trusted adapters. Resolved values are private
//! to the caller and never exported into the daemon's process environment.
use anyhow::{Context, Result};
use fridica_core::secret::{KeyringRef, SecretStore};
use std::ffi::OsStr;

pub use fridica_core::secret::NativeStore;

/// An explicitly supplied environment credential takes precedence, including
/// in existing service units. Only consult the keyring for a configured URI.
pub fn resolve(
    store: &dyn SecretStore,
    variable: &str,
    env: Option<&OsStr>,
    reference: Option<&KeyringRef>,
) -> Result<String> {
    if let Some(value) = env {
        return Ok(value
            .to_str()
            .context("credential must be UTF-8")?
            .to_owned());
    }
    let reference =
        reference.with_context(|| format!("set {variable} or configure a keyring reference"))?;
    let bytes = store
        .get(reference.name())
        .with_context(|| format!("cannot read {} for {variable}", reference.name()))?
        .with_context(|| {
            format!(
                "missing {variable}; run fridica secret set {}",
                reference.name()
            )
        })?;
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("stored credential must be UTF-8"))
}

/// Select references for setup without reading any secret values. A session
/// without an accessible keyring can still use environment or gh credentials.
pub fn starter_config() -> String {
    crate::config::setup::starter_with(
        &NativeStore.list().unwrap_or_default(),
        |name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()),
        || {
            use std::process::{Command, Stdio};
            // gh reads its locally stored login. Its token never enters our output.
            Command::new("gh")
                .args(["auth", "token", "--hostname", "github.com"])
                .env_remove("GH_TOKEN")
                .env_remove("GITHUB_TOKEN")
                .env("GH_PROMPT_DISABLED", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        },
    )
}

/// Supply only the standard worker tokens requested by attached MCP servers.
pub(crate) fn worker_tokens(
    environment: &mut std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    servers: &[fridica_agent::McpServer],
    lookup: impl Fn(&str) -> Result<Option<Vec<u8>>>,
) -> Result<()> {
    for (_, variable, entry) in crate::config::setup::MCP_TOKENS {
        if !servers
            .iter()
            .any(|server| server.env_vars.iter().any(|v| v == variable))
        {
            continue;
        }
        let present = |name: &str| {
            environment
                .get(OsStr::new(name))
                .filter(|v| v.to_str().is_some_and(|s| !s.trim().is_empty()))
        };
        if present(variable).is_some() {
            continue;
        }
        if variable == "GH_TOKEN" {
            if let Some(token) = present("GITHUB_TOKEN").cloned() {
                environment.insert(variable.into(), token);
                continue;
            }
        }
        environment.remove(OsStr::new(variable));
        if let Some(bytes) = lookup(entry)? {
            let token = String::from_utf8(bytes).context("worker credential must be UTF-8")?;
            environment.insert(variable.into(), token.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_resolves_only_requested_tokens_with_environment_precedence() {
        let servers = [
            fridica_agent::scix::scix("fridica"),
            fridica_agent::overleaf::overleaf("fridica"),
        ];
        let mut environment = std::collections::BTreeMap::from([
            ("ADS_TOKEN".into(), "env-ads".into()),
            ("OVERLEAF_GIT_TOKEN".into(), " ".into()),
        ]);
        worker_tokens(&mut environment, &servers, |entry| {
            assert_eq!(entry, "fridica/overleaf/git-token");
            Ok(Some(b"stored-overleaf".to_vec()))
        })
        .unwrap();
        assert_eq!(environment[OsStr::new("ADS_TOKEN")], "env-ads");
        assert_eq!(
            environment[OsStr::new("OVERLEAF_GIT_TOKEN")],
            "stored-overleaf"
        );
        assert!(!environment.contains_key(OsStr::new("GH_TOKEN")));
    }
}
