//! Validated, comment-preserving edits. The caller journals intent before commit
//! and owns serialization; this module never opens the database.
use super::{loader, Config, LoadContext};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path};
use toml_edit::{DocumentMut, Item, Table};

pub const PARENT_KEYS: &[&str] = &["backend", "model", "triage_model", "reasoning_effort"];
pub const LIMIT_KEYS: &[&str] = &[
    "max_delegations_per_turn",
    "max_workers_per_thread",
    "max_jobs",
    "job_timeout",
    "worker_idle",
];

pub fn valid_patch(section: &str, changes: &Value) -> bool {
    let allowed = match section {
        "parent" => PARENT_KEYS,
        "limits" => LIMIT_KEYS,
        _ => return false,
    };
    changes
        .as_object()
        .is_some_and(|m| m.keys().all(|k| allowed.contains(&k.as_str())))
}

#[derive(Debug)]
pub struct InvalidPatch;
impl std::fmt::Display for InvalidPatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid configuration values")
    }
}
impl std::error::Error for InvalidPatch {}

pub struct Prepared {
    pub config: Config,
    pub before: String,
    source: String,
}
fn fingerprint(source: &str) -> String {
    format!("{:x}", Sha256::digest(source.as_bytes()))
}
pub fn read(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        bail!("configuration must be a regular file of at most 1 MiB");
    }
    Ok(fs::read_to_string(path)?)
}
impl Prepared {
    pub fn new(
        current: &Config,
        section: &str,
        changes: &Value,
        context: &LoadContext,
    ) -> Result<Self> {
        if !valid_patch(section, changes) {
            bail!("configuration field is not editable here");
        }
        let source = read(&current.path)?;
        if fingerprint(&source) != current.fingerprint {
            bail!("configuration changed outside the daemon; restart before editing");
        }
        if changes.as_object().unwrap().is_empty() {
            return Ok(Self {
                config: current.clone(),
                before: current.fingerprint.clone(),
                source,
            });
        }
        let mut document = source.parse::<DocumentMut>()?;
        if document.get(section).is_none() {
            document[section] = Item::Table(Table::new());
        }
        let table = document[section]
            .as_table_like_mut()
            .context("configuration section must be a table")?;
        for (key, value) in changes.as_object().unwrap() {
            let mut value = match value {
                Value::String(v) => toml_edit::Value::from(v.as_str()),
                Value::Number(v) if v.is_i64() => toml_edit::Value::from(v.as_i64().unwrap()),
                Value::Number(v) => toml_edit::Value::from(v.as_f64().context("invalid number")?),
                Value::Bool(v) => toml_edit::Value::from(*v),
                _ => return Err(InvalidPatch.into()),
            };
            if let Some(old) = table.get(key).and_then(Item::as_value) {
                *value.decor_mut() = old.decor().clone();
            }
            table.insert(key, Item::Value(value));
        }
        let source = document.to_string();
        if source.len() > 1024 * 1024 {
            bail!("edited configuration exceeds 1 MiB");
        }
        let config = loader::parse(&source, &current.path, context).map_err(|_| InvalidPatch)?;
        // Loading may resolve inherited defaults (notably machine backends).
        // A live edit must never silently change worker inventory or policy.
        let stable = |config: &Config| -> Result<Value> {
            let mut value = serde_json::to_value(config)?;
            value.as_object_mut().unwrap().remove("fingerprint");
            for field in changes.as_object().unwrap().keys() {
                value[section].as_object_mut().unwrap().remove(field);
            }
            Ok(value)
        };
        if stable(&config)? != stable(current)? {
            bail!("configuration edit changes inherited inventory or other settings; restart required");
        }
        Ok(Self {
            config,
            before: current.fingerprint.clone(),
            source,
        })
    }
    /// Atomic replacement plus file/directory durability. Recheck immediately
    /// before rename so an edit made since preparation is refused, not clobbered.
    pub fn commit(&self) -> Result<()> {
        let path = &self.config.path;
        let parent = path.parent().context("configuration has no directory")?;
        let mut file = tempfile::Builder::new()
            .prefix(".fridica-config-")
            .tempfile_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(self.source.as_bytes())?;
        file.as_file().sync_all()?;
        if fingerprint(&read(path)?) != self.before {
            bail!("configuration changed before replacement");
        }
        file.persist(path)?;
        sync_directory(path)?;
        Ok(())
    }
}
pub fn disk_fingerprint(path: &Path) -> Result<String> {
    Ok(fingerprint(&read(path)?))
}
pub fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path.parent().context("configuration has no directory")?)?.sync_all()?;
    Ok(())
}
