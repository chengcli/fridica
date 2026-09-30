//! Path-free repository facts shared with every worker and parent call.
use super::contract::{read, trim, LIMIT};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{collections::HashSet, path::Path};
use toml_edit::{DocumentMut, TableLike};
pub const DEFAULT: &str = include_str!("../../assets/repos.toml");
#[derive(Clone, Serialize)]
pub struct Repo {
    pub name: String,
    pub url: String,
    pub collaborators: Vec<String>,
    pub owner: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub notes: String,
}
fn string<'a>(table: &'a dyn TableLike, key: &str) -> Result<&'a str> {
    table
        .get(key)
        .and_then(|v| v.as_str())
        .with_context(|| format!("repository {key} must be a string"))
}
pub fn parse(text: &str) -> Result<Vec<Repo>> {
    if text.len() > LIMIT {
        bail!("repository list exceeds 64 KiB");
    }
    // Do not include TOML source excerpts in diagnostics; owner notes can be private.
    let doc = text
        .parse::<DocumentMut>()
        .map_err(|_| anyhow::anyhow!("repository list is not valid TOML"))?;
    if doc.iter().any(|(k, _)| k != "repos") {
        bail!("repository list must contain only [[repos]] entries");
    }
    let Some(item) = doc.get("repos") else {
        return Ok(vec![]);
    };
    let tables: Vec<&dyn TableLike> = if let Some(array) = item.as_array_of_tables() {
        array.iter().map(|t| t as &dyn TableLike).collect()
    } else if let Some(array) = item.as_array() {
        array
            .iter()
            .map(|v| {
                v.as_inline_table()
                    .map(|t| t as &dyn TableLike)
                    .context("repository entry must be a table")
            })
            .collect::<Result<_>>()?
    } else {
        bail!("repository list must contain only [[repos]] entries")
    };
    let mut seen = HashSet::new();
    let mut repos = Vec::new();
    for table in tables {
        if table
            .iter()
            .any(|(k, _)| !["name", "url", "collaborators", "notes"].contains(&k))
        {
            bail!("repository entry has unknown fields; local paths are deliberately not part of the shared list");
        }
        let name = trim(string(table, "name")?);
        if name.is_empty()
            || name.len() > 64
            || !name.as_bytes()[0].is_ascii_alphanumeric()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._ -".contains(&c))
        {
            bail!("repository needs a name of letters, digits, dots, dashes, or spaces");
        }
        let url = trim(string(table, "url").context("repository needs an https URL")?);
        let valid_url = url
            .strip_prefix("https://")
            .and_then(|s| s.split_once('/'))
            .is_some_and(|(host, path)| {
                !host.is_empty()
                    && host
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
                    && !path.is_empty()
                    && !path.chars().any(super::contract::whitespace)
            });
        if !valid_url {
            bail!("repository needs an https URL");
        }
        let notes = if table.contains_key("notes") {
            trim(string(table, "notes")?).to_string()
        } else {
            String::new()
        };
        let array = table
            .get("collaborators")
            .and_then(|v| v.as_array())
            .context("collaborators must list at least the owner first")?;
        let collaborators = array
            .iter()
            .map(|v| {
                v.as_str()
                    .map(trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .context("collaborators must list at least the owner first")
            })
            .collect::<Result<Vec<_>>>()?;
        let owner = collaborators
            .first()
            .context("collaborators must list at least the owner first")?
            .clone();
        if !seen.insert(name.to_lowercase()) {
            bail!("repository name is listed twice");
        }
        repos.push(Repo {
            name: name.into(),
            url: url.into(),
            collaborators,
            owner,
            notes,
        });
    }
    Ok(repos)
}
pub fn load(path: Option<&Path>) -> Result<Vec<Repo>> {
    match path {
        None => parse(DEFAULT),
        Some(path) => parse(&read(path, "repository list")?)
            .with_context(|| format!("invalid repository list {}", path.display())),
    }
}
