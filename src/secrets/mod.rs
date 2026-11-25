//! Native credential access for trusted adapters. Resolved values are private
//! to the caller and never exported into the daemon's process environment.
use anyhow::{Context, Result};
use fridica_core::secret::{KeyringRef, SecretStore};
use std::ffi::OsStr;

pub use fridica_core::secret::NativeStore;

/// Explicit references take priority. Standard Slack variables also have a
/// conventional keyring entry when the configuration omits the reference.
pub(crate) fn keyring_name<'a>(
    variable: &str,
    reference: Option<&'a KeyringRef>,
) -> Option<&'a str> {
    reference.map(KeyringRef::name).or(match variable {
        "SLACK_APP_TOKEN" => Some("fridica/slack/app-token"),
        "SLACK_USER_TOKEN" => Some("fridica/slack/user-token"),
        _ => None,
    })
}

/// An explicitly supplied environment credential takes precedence, including
/// in existing service units. Otherwise read the configured or conventional keyring entry.
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
    let name = keyring_name(variable, reference)
        .with_context(|| format!("set {variable} or configure a keyring reference"))?;
    let bytes = store
        .get(name)
        .with_context(|| format!("cannot read {name} for {variable}"))?
        .with_context(|| format!("missing {variable}; run fridica secret set {name}"))?;
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
    native_allowed: bool,
    lookup: impl Fn(&str) -> Result<Option<Vec<u8>>>,
) -> Result<()> {
    use crate::config::overleaf::{NATIVE_ENTRY, NATIVE_ENV};
    let native_allowed = native_allowed
        && servers
            .iter()
            .any(|s| s.name == "overleaf" && s.env_vars.iter().any(|n| n == NATIVE_ENV));
    if !native_allowed {
        environment.remove(OsStr::new(NATIVE_ENV));
    }
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
    if native_allowed {
        let raw = environment
            .get(OsStr::new(NATIVE_ENV))
            .and_then(|v| v.to_str())
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned);
        let raw = match raw {
            Some(value) => Some(value),
            None => lookup(NATIVE_ENTRY)?
                .map(String::from_utf8)
                .transpose()
                .map_err(|_| anyhow::anyhow!("invalid native session encoding"))?,
        };
        environment.remove(OsStr::new(NATIVE_ENV));
        if let Some(raw) = raw {
            crate::config::overleaf::validate_session(&raw)
                .map_err(|_| anyhow::anyhow!("native session invalid or outside owner scope"))?;
            environment.insert(NATIVE_ENV.into(), raw.into());
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
        worker_tokens(&mut environment, &servers, false, |entry| {
            if entry == "fridica/overleaf/native-session" {
                return Ok(None);
            }
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

    #[test]
    fn native_default_deny_blocks_ambient_and_lookup() {
        let servers = [fridica_agent::overleaf::overleaf("fridica")];
        let mut environment = std::collections::BTreeMap::from([
            ("OVERLEAF_NATIVE_SESSION".into(), "fixture-ambient".into()),
            ("ADS_TOKEN".into(), "fixture-ads".into()),
            ("OVERLEAF_GIT_TOKEN".into(), "fixture-git".into()),
        ]);
        worker_tokens(&mut environment, &servers, false, |_| {
            panic!("unauthorized lookup")
        })
        .unwrap();
        assert!(!environment.contains_key(OsStr::new("OVERLEAF_NATIVE_SESSION")));
        assert_eq!(environment[OsStr::new("OVERLEAF_GIT_TOKEN")], "fixture-git");
    }
    #[test]
    fn native_bound_lookup_uses_only_the_dedicated_entry_and_exact_scope() {
        let mut environment = std::collections::BTreeMap::from([
            ("OVERLEAF_GIT_TOKEN".into(), "fixture-git".into()),
            ("ADS_TOKEN".into(), "fixture-ads".into()),
        ]);
        let mut server = fridica_agent::overleaf::overleaf("fridica");
        server.env_vars.push("OVERLEAF_NATIVE_SESSION".into());
        let raw = serde_json::json!({"schema":2,"cookie":"overleaf_session2=fixture", "account_id":"111111111111111111111111", "expected_email":"owner@example.invalid", "projects":["333333333333333333333333"],"operations":crate::config::overleaf::OPERATIONS,"expires_at":crate::config::overleaf::now()+600}).to_string();
        worker_tokens(&mut environment, &[server], true, |entry| {
            assert_eq!(entry, crate::config::overleaf::NATIVE_ENTRY);
            Ok(Some(raw.as_bytes().to_vec()))
        })
        .unwrap();
        assert_eq!(
            environment[OsStr::new("OVERLEAF_NATIVE_SESSION")],
            raw.as_str()
        );
        assert!(crate::config::overleaf::validate_session(
            &raw.replace("333333333333333333333333", "invalid-project")
        )
        .is_err());
    }
}
