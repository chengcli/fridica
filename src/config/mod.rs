//! Configuration transformations preserve comments and never silently discard keys.
pub mod contract;
pub mod editor;
pub mod egress;
pub mod isolation;
pub mod loader;
pub mod provisions;
pub mod repos;
pub mod schema;
pub mod setup;
pub use fridica_core::config::{registry, Attention};
pub use loader::{load, LoadContext};
pub use schema::Config;

use anyhow::{bail, Context, Result};
use toml_edit::{value, DocumentMut, Item, Table};

pub fn migrate_text(source: &str) -> Result<String> {
    let mut doc = source
        .parse::<DocumentMut>()
        .context("invalid TOML configuration")?;
    let explicit = doc.get("attention").and_then(|v| v.get("streak_signal"));
    if let Some(item) = explicit {
        if item.as_integer().is_none_or(|v| v <= 0) {
            bail!("attention.streak_signal must be a positive integer");
        }
    }
    let has_override = explicit.is_some();
    let mut legacy = Vec::new();
    let mut comments = String::new();
    if let Some(limits) = doc.get_mut("limits") {
        let table = limits.as_table_mut().context("limits must be a table")?;
        for key in ["max_wait_replies", "max_no_progress"] {
            if let Some((decorated, _)) = table.get_key_value(key) {
                for raw in [
                    decorated.leaf_decor().prefix(),
                    decorated.leaf_decor().suffix(),
                ]
                .into_iter()
                .flatten()
                {
                    if let Some(s) = raw.as_str() {
                        if s.contains('#') {
                            comments.push_str(s.trim());
                            comments.push('\n');
                        }
                    }
                }
            }
            if let Some(item) = table.remove(key) {
                let number = item
                    .as_integer()
                    .filter(|v| *v > 0)
                    .context("legacy loop limits must be positive integers")?;
                legacy.push(number);
                // Removed values may carry user comments. Retain them on the new setting.
                if let Some(v) = item.as_value() {
                    for raw in [v.decor().prefix(), v.decor().suffix()]
                        .into_iter()
                        .flatten()
                    {
                        if let Some(s) = raw.as_str() {
                            if s.contains('#') {
                                comments.push_str(s.trim());
                                comments.push('\n');
                            }
                        }
                    }
                }
            }
        }
    }
    if !legacy.is_empty() {
        // Both Python limits defaulted to three. If only one was configured,
        // the omitted limit still constrained the old runtime.
        if legacy.len() == 1 {
            legacy.push(3);
        }
        if doc.get("attention").is_none() {
            doc["attention"] = Item::Table(Table::new());
        }
        let table = doc["attention"]
            .as_table_mut()
            .context("attention must be a table")?;
        if !has_override {
            table.insert("streak_signal", value(*legacy.iter().min().unwrap()));
        }
        if !comments.is_empty() {
            let key = table
                .key_mut("streak_signal")
                .context("missing streak_signal")?;
            // Key decoration holds full-line comments; value suffix retains inline comments.
            let existing = key
                .leaf_decor()
                .prefix()
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let mut key = key;
            key.leaf_decor_mut()
                .set_prefix(format!("{comments}{existing}"));
        }
    }
    Ok(doc.to_string())
}
