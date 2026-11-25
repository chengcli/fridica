use std::collections::BTreeSet;
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

fn matching_names(store: &dyn SecretStore, patterns: &[String]) -> Result<Vec<String>> {
    let names = stored_names(store)?;
    if patterns.is_empty() {
        ensure!(!names.is_empty(), "no stored Fridica credentials to copy");
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
        ensure!(
            matches.peek().is_some(),
            "no stored credential matches {pattern}"
        );
        selected.extend(matches.cloned());
    }
    Ok(selected.into_iter().collect())
}

pub(super) fn copy(store: &dyn SecretStore, destination: &str, names: &[String]) -> Result<()> {
    copy_with_ssh(store, destination, names, Command::new("ssh"))
}

fn copy_with_ssh(
    store: &dyn SecretStore,
    destination: &str,
    names: &[String],
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
    let mut entries = Vec::new();
    for name in matching_names(store, names)? {
        let value = store
            .get(&name)?
            .with_context(|| format!("missing {name}"))?;
        entries.push(Entry { name, value });
    }
    let payload = serde_json::to_vec(&entries)?;
    let mut child = ssh
        .args(["-T", "--", destination, "fridica secret receive"])
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
            matching_names(&source, &[])?,
            [
                "fridica/ads/token",
                "fridica/slack/app-token",
                "fridica/slack/user-token"
            ]
        );
        assert_eq!(
            matching_names(&source, &["slack/*".into(), "fridica/slack/*-token".into()])?,
            ["fridica/slack/app-token", "fridica/slack/user-token"]
        );
        assert!(matching_names(&source, &["ads/*".into(), "missing/*".into()]).is_err());
        Ok(())
    }

    #[test]
    fn copy_sends_complete_entries_only_on_stdin() -> Result<()> {
        let source = Store::default();
        source.set("fridica/slack/app-token", b"fixture\nwith spaces")?;
        source.set("fridica/test/binary", &[0, 255])?;
        let capture = tempfile::NamedTempFile::new()?;
        let mut ssh = Command::new("sh");
        ssh.args(["-c", r#"test "$#" = 4 && test "$1" = -T && test "$2" = -- && test "$3" = owner@host && test "$4" = 'fridica secret receive' && cat > "$CAPTURE""#, "ssh"])
            .env("CAPTURE", capture.path());
        copy_with_ssh(
            &source,
            "owner@host",
            &["slack/app-token".into(), "fridica/test/binary".into()],
            ssh,
        )?;
        let bytes = std::fs::read(capture.path())?;
        let target = Store::default();
        assert!(receive(&target, &bytes[..bytes.len() - 1]).is_err());
        assert!(target.0.lock().unwrap().is_empty());
        receive(&target, bytes.as_slice())?;
        assert_eq!(*source.0.lock().unwrap(), *target.0.lock().unwrap());
        Ok(())
    }
}
