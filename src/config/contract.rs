//! Owner-editable rule sections, reloaded for every instruction build.
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{collections::HashMap, io::Read, path::Path};
pub const LIMIT: usize = 64 * 1024;
pub const DEFAULT: &str = include_str!("../fridica/parent/contract.md");
pub fn whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}
pub fn trim(s: &str) -> &str {
    s.trim_matches(whitespace)
}
pub fn read(path: &Path, label: &str) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot read {label} {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((LIMIT + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        bail!("{label} exceeds 64 KiB");
    }
    String::from_utf8(bytes).with_context(|| format!("{label} must be UTF-8"))
}
#[derive(Clone, Default, Serialize)]
pub struct Contract {
    pub participation: String,
    pub replies: String,
    pub delegation: String,
    pub workers: String,
    pub debriefs: String,
    pub extra: String,
}
impl Contract {
    pub fn parent(&self) -> String {
        join(&[&self.replies, &self.delegation, &self.extra])
    }
    pub fn worker(&self) -> String {
        join(&[&self.workers, &self.extra])
    }
}
fn join(parts: &[&String]) -> String {
    parts
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}
pub fn parse(text: &str) -> Result<Contract> {
    if text.len() > LIMIT {
        bail!("contract exceeds 64 KiB");
    }
    // Match the frozen ^##\s+(.+?)\s*$ section grammar, including CRLF,
    // aliases and a bare ## followed by a title on the next nonempty line.
    let mut headings = Vec::new();
    let mut offset = 0;
    let mut consumed = 0;
    for line in text.split_inclusive('\n') {
        if offset >= consumed
            && line.starts_with("##")
            && text[offset + 2..].chars().next().is_some_and(whitespace)
        {
            let rest = text[offset + 2..].trim_start_matches(whitespace);
            let start = text.len() - rest.len();
            let end = start + rest.find('\n').unwrap_or(rest.len());
            let title = trim(&text[start..end]);
            if !title.is_empty() {
                consumed = end;
                headings.push((offset, end, trim(title.trim_start_matches('#'))));
            } else if rest.is_empty() && text[offset + 2..].chars().skip(1).any(|c| c != '\n') {
                // Python's greedy whitespace prefix can backtrack to leave
                // one non-newline whitespace character as an empty title.
                consumed = text.len();
                headings.push((offset, text.len(), ""));
            }
        }
        offset += line.len();
    }
    let mut found = HashMap::new();
    let mut extra = Vec::new();
    for (index, (_, end, title)) in headings.iter().enumerate() {
        let stop = headings.get(index + 1).map_or(text.len(), |h| h.0);
        let body = trim(&text[*end..stop]);
        let key = match title.to_lowercase().as_str() {
            "participation" | "triage" => "participation",
            "replies" | "reply" => "replies",
            "delegation" => "delegation",
            "worker reports" | "workers" | "worker" => "workers",
            "debriefs" | "debrief" => "debriefs",
            _ => "",
        };
        if key.is_empty() {
            if !body.is_empty() {
                extra.push(format!("## {title}\n\n{body}"));
            }
        } else {
            found.entry(key).or_insert(body.to_string());
        }
    }
    let missing = [("participation", "Participation"), ("replies", "Replies")]
        .into_iter()
        .filter(|(key, _)| found.get(key).is_none_or(String::is_empty))
        .map(|(_, title)| format!("## {title}"))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "contract is missing or has an empty section: {}",
            missing.join(", ")
        );
    }
    Ok(Contract {
        participation: found.remove("participation").unwrap_or_default(),
        replies: found.remove("replies").unwrap_or_default(),
        delegation: found.remove("delegation").unwrap_or_default(),
        workers: found.remove("workers").unwrap_or_default(),
        debriefs: found.remove("debriefs").unwrap_or_default(),
        extra: extra.join("\n\n"),
    })
}
pub fn load(path: Option<&Path>) -> Result<Contract> {
    let defaults = parse(DEFAULT)?;
    let Some(path) = path else {
        return Ok(defaults);
    };
    let mut contract = parse(&read(path, "contract")?)
        .with_context(|| format!("invalid contract {}", path.display()))?;
    if contract.delegation.is_empty() {
        contract.delegation = defaults.delegation;
    }
    if contract.workers.is_empty() {
        contract.workers = defaults.workers;
    }
    if contract.debriefs.is_empty() {
        contract.debriefs = defaults.debriefs;
    }
    Ok(contract)
}
