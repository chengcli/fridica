use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

use super::{reference, stored_names, SecretStore};

#[derive(Serialize, Deserialize)]
struct Entry {
    name: String,
    value: Vec<u8>,
}

fn matching_names(names: Vec<String>, patterns: &[String]) -> Result<Vec<String>> {
    if patterns.is_empty() {
        return Ok(names);
    }
    let mut selected = BTreeSet::new();
    for pattern in patterns {
        // Apply the same path rules as exact names, allowing '*' as a wildcard.
        reference(&pattern.replace('*', "x"))?;
        let path = pattern.strip_prefix("fridica/").unwrap_or(pattern);
        let expression = regex::escape(path).replace(r"\*", ".*");
        let matcher = regex::Regex::new(&format!("^fridica/{expression}$"))?;
        let mut matches = names
            .iter()
            .filter(|name| matcher.is_match(name))
            .peekable();
        ensure!(matches.peek().is_some(), "no credential matches {pattern}");
        selected.extend(matches.cloned());
    }
    Ok(selected.into_iter().collect())
}

fn environment_sources(is_set: impl Fn(&str) -> bool) -> BTreeMap<String, &'static str> {
    let mut sources = BTreeMap::new();
    for (name, variable) in [
        ("fridica/slack/app-token", "SLACK_APP_TOKEN"),
        ("fridica/slack/user-token", "SLACK_USER_TOKEN"),
    ]
    .into_iter()
    .chain(crate::config::setup::MCP_TOKENS.map(|(_, var, name)| (name, var)))
    .chain([("fridica/github/mcp-token", "GITHUB_TOKEN")])
    {
        if is_set(variable) {
            sources.entry(name.to_owned()).or_insert(variable);
        }
    }
    sources
}

pub(super) fn copy(
    store: &dyn SecretStore,
    destination: &str,
    patterns: &[String],
    from_env: bool,
    remote_bin: Option<&str>,
) -> Result<()> {
    let entries = resolve_entries(store, patterns, from_env, |var| std::env::var_os(var))?;
    send(destination, &entries, remote_bin, Command::new("ssh"))
}

fn resolve_entries(
    store: &dyn SecretStore,
    patterns: &[String],
    from_env: bool,
    getenv: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Vec<Entry>> {
    let sources = environment_sources(|var| getenv(var).is_some_and(|v| !v.is_empty()));
    let mut names: BTreeSet<_> = sources.keys().cloned().collect();
    if !from_env {
        match stored_names(store) {
            Ok(stored) => names.extend(stored),
            Err(err) if !sources.is_empty() => {
                eprintln!("Could not list the source keyring ({err}); checking environment variables only.");
            }
            Err(err) => return Err(err),
        }
    }
    ensure!(
        !names.is_empty(),
        "no Fridica credentials found in the selected sources"
    );
    let mut entries = Vec::new();
    for name in matching_names(names.into_iter().collect(), patterns)? {
        let value = match sources.get(&name) {
            Some(variable) => getenv(variable)
                .and_then(|value| value.into_string().ok())
                .with_context(|| format!("cannot read {variable} as text"))?
                .into_bytes(),
            None => store
                .get(&name)?
                .with_context(|| format!("missing {name}"))?,
        };
        entries.push(Entry { name, value });
    }
    Ok(entries)
}

fn send(
    destination: &str,
    entries: &[Entry],
    remote_bin: Option<&str>,
    mut ssh: Command,
) -> Result<()> {
    ensure!(
        !destination.is_empty()
            && !destination.starts_with('-')
            && destination
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"@._-:[]%".contains(&b)),
        "expected an SSH host alias or user@host"
    );
    use crate::exec::shell::quote;
    let command = match remote_bin {
        Some(path) => {
            ensure!(
                std::path::Path::new(path).is_absolute(),
                "--remote-bin requires an absolute remote path"
            );
            format!("exec {} secret receive", quote(path))
        }
        None => format!("exec sh -c {}", quote(include_str!("find-receiver.sh"))),
    };
    let payload = serde_json::to_vec(entries)?;
    let mut child = ssh
        .args(["-T", "--", destination, &command])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("start ssh")?;
    let sent = child
        .stdin
        .take()
        .expect("piped ssh stdin")
        .write_all(&payload);
    let status = child.wait().context("wait for ssh")?;
    ensure!(
        status.success(),
        "remote credential transfer failed ({status})"
    );
    sent.context("send credentials over ssh")?;
    eprintln!("Copied {} credential(s) to {destination}", entries.len());
    for entry in entries {
        eprintln!("  {}", entry.name);
    }
    Ok(())
}

pub(super) fn receive(store: &dyn SecretStore, input: impl Read) -> Result<()> {
    // Parse the complete message before updating the keyring. Parser diagnostics
    // can quote input, so keep them out of the error reported to the terminal.
    let entries: Vec<Entry> = serde_json::from_reader(input)
        .map_err(|_| anyhow::anyhow!("invalid or incomplete credential transfer"))?;
    for entry in &entries {
        ensure!(
            reference(&entry.name)? == entry.name,
            "expected a full credential name"
        );
    }
    for entry in entries {
        store
            .set(&entry.name, &entry.value)
            .with_context(|| format!("store {}", entry.name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, sync::Mutex};

    #[derive(Default)]
    struct Store(Mutex<BTreeMap<String, Vec<u8>>>);

    impl SecretStore for Store {
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(name).cloned())
        }
        fn set(&self, name: &str, value: &[u8]) -> Result<()> {
            self.0.lock().unwrap().insert(name.into(), value.into());
            Ok(())
        }
        fn list(&self) -> Result<Vec<String>> {
            Ok(self.0.lock().unwrap().keys().cloned().collect())
        }
        fn delete(&self, _: &str) -> Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn environment_sources_select_known_tokens_and_prefer_gh_token() {
        let names = environment_sources(|var| {
            ["GH_TOKEN", "GITHUB_TOKEN", "ADS_TOKEN", "PATH"].contains(&var)
        });
        assert_eq!(names.len(), 2);
        assert_eq!(names["fridica/github/mcp-token"], "GH_TOKEN");
        assert_eq!(names["fridica/ads/token"], "ADS_TOKEN");
        assert_eq!(
            environment_sources(|var| var == "GITHUB_TOKEN")["fridica/github/mcp-token"],
            "GITHUB_TOKEN"
        );
    }

    #[test]
    fn default_copy_combines_sources_and_prefers_environment_per_name() -> Result<()> {
        let store = Store::default();
        store.set("fridica/slack/app-token", b"superseded")?;
        store.set("fridica/ads/token", b"stored")?;
        let entries = resolve_entries(&store, &[], false, |variable| match variable {
            "SLACK_APP_TOKEN" | "GH_TOKEN" => Some("exported".into()),
            "ADS_TOKEN" => Some("".into()),
            _ => None,
        })?;
        let values: Vec<_> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_slice()))
            .collect();
        assert_eq!(
            values,
            [
                ("fridica/ads/token", b"stored".as_slice()),
                ("fridica/github/mcp-token", b"exported"),
                ("fridica/slack/app-token", b"exported"),
            ]
        );
        Ok(())
    }

    #[test]
    fn discovery_filters_and_deduplicates_stored_names() -> Result<()> {
        let source = Store::default();
        for name in [
            "fridica/slack/app-token",
            "fridica/slack/user-token",
            "fridica/ads/token",
            "unrelated",
        ] {
            source.set(name, b"fixture")?;
        }
        assert_eq!(
            matching_names(stored_names(&source)?, &[])?,
            [
                "fridica/ads/token",
                "fridica/slack/app-token",
                "fridica/slack/user-token"
            ]
        );
        assert_eq!(
            matching_names(
                stored_names(&source)?,
                &["slack/*".into(), "fridica/slack/*-token".into()]
            )?,
            ["fridica/slack/app-token", "fridica/slack/user-token"]
        );
        assert!(matching_names(
            stored_names(&source)?,
            &["ads/*".into(), "missing/*".into()]
        )
        .is_err());
        Ok(())
    }

    #[test]
    fn copy_sends_complete_entries_only_on_stdin() -> Result<()> {
        let source = Store::default();
        source.set("fridica/slack/app-token", b"fixture\nwith spaces")?;
        source.set("fridica/test/binary", &[0, 255])?;
        let capture = tempfile::NamedTempFile::new()?;
        let mut ssh = Command::new("sh");
        let home = tempfile::tempdir()?;
        let bin = home.path().join("pyenv/bin");
        std::fs::create_dir_all(&bin)?;
        let receiver = bin.join("fridica");
        std::fs::write(
            &receiver,
            r#"#!/bin/sh
[ "$1 $2" = "secret receive" ] || exit 2
if [ "$3" = "--help" ]; then
    cat >/dev/null
    exit 0
fi
cat > "$CAPTURE"
"#,
        )?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&receiver, std::fs::Permissions::from_mode(0o700))?;
        ssh.args(["-c", r#"test "$#" = 4 && test "$1" = -T && test "$2" = -- && test "$3" = owner@host && exec sh -c "$4""#, "ssh"])
            .env_clear().env("HOME", home.path()).env("PATH", "/usr/bin:/bin").env("CAPTURE", capture.path());
        let entries = stored_names(&source)?
            .into_iter()
            .map(|name| Entry {
                value: source.get(&name).unwrap().unwrap(),
                name,
            })
            .collect::<Vec<_>>();
        send("owner@host", &entries, None, ssh)?;
        let bytes = std::fs::read(capture.path())?;
        let target = Store::default();
        assert!(receive(&target, &bytes[..bytes.len() - 1]).is_err());
        assert!(target.0.lock().unwrap().is_empty());
        receive(&target, bytes.as_slice())?;
        assert_eq!(*source.0.lock().unwrap(), *target.0.lock().unwrap());
        Ok(())
    }
}
