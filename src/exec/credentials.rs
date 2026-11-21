//! Owner-selected integration credentials, transported only in process environment
//! and encrypted SSH stdin. Master credentials are always filtered first.
use super::{
    process::{self, Launch},
    shell,
};
use anyhow::{bail, Result};
use std::collections::BTreeMap;

pub const HELPER: &str = include_str!("credentials.py");

fn token_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    matches!(name.as_str(), "TOKEN" | "API_KEY" | "SECRET" | "PASSWORD")
        || [
            "_TOKEN",
            "_KEY",
            "_PAT",
            "_API_KEY",
            "_ACCESS_KEY_ID",
            "_SECRET_ACCESS_KEY",
            "_SECRET",
            "_PASSWORD",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// The launch must already be an SSH launch. Values are never copied into argv.
/// Explicit MCP variable names cover nonstandard credential names, but local
/// configuration directory paths are intentionally not sent to another host.
pub fn forward(mut launch: Launch, excluded: &[String], mcp_names: &[String]) -> Result<Launch> {
    let clean = process::scrubbed_environment(launch.env.clone(), excluded, &BTreeMap::new());
    launch.env = clean.clone();
    let mut names = Vec::new();
    let mut size = 0;
    for (key, value) in clean {
        let Some(name) = key.to_str() else {
            continue;
        };
        if !shell::env_name(name)
            || name == "GH_CONFIG_DIR"
            || !(token_name(name) || mcp_names.iter().any(|n| n == name))
        {
            continue;
        }
        let Some(value) = value.to_str() else {
            bail!("worker credential must be UTF-8");
        };
        if value.is_empty() {
            continue;
        }
        size += name.len() + value.len();
        if size > 32768 || value.contains('\0') {
            bail!("worker credentials exceed transfer bounds");
        }
        names.push(name.to_owned());
    }
    if names.is_empty() {
        return Ok(launch);
    }
    if launch.argv.first().map(String::as_str) != Some("ssh") {
        bail!("credential forwarding requires SSH");
    }
    let script = launch
        .argv
        .last_mut()
        .ok_or_else(|| anyhow::anyhow!("missing SSH command"))?;
    *script = shell::join(&crate::exec::helper::argv(vec![
        HELPER.into(),
        "receive".into(),
        script.clone(),
    ]));
    let mut wrapper = crate::exec::helper::argv(vec![
        HELPER.into(),
        "send".into(),
        serde_json::to_string(&names)?,
    ]);
    wrapper.extend(launch.argv);
    launch.argv = wrapper;
    Ok(launch)
}

/// Insertion point for SSH options, including launches wrapped for credential stdin.
pub(crate) fn ssh_options_index(launch: &Launch) -> usize {
    if launch.argv.get(4).is_some_and(|s| s == HELPER)
        && launch.argv.get(5).is_some_and(|s| s == "send")
    {
        8
    } else {
        1
    }
}
