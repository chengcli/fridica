//! Placement is pure: choosing a sticky machine does not reserve a job slot or process.
pub mod probe;
use crate::config::registry::{Machine, Registry, Workspace};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Selector {
    pub machine: String,
    pub tags: Vec<String>,
    pub workspace: String,
    pub backend: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchError {
    pub message: String,
    pub candidates: Vec<String>,
}
impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for MatchError {}
#[derive(Debug)]
pub struct Placement<'a> {
    pub machine: &'a Machine,
    pub workspace: &'a Workspace,
    pub backend: String,
}

fn error(message: String, candidates: Vec<String>) -> MatchError {
    MatchError {
        message,
        candidates,
    }
}
fn quoted(s: &str) -> String {
    // Configured names are ASCII. Keep Python's repr choice for diagnostic parity.
    if s.contains('\'') && !s.contains('"') {
        format!("\"{}\"", s.replace('\\', "\\\\"))
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}
pub fn resolve<'a>(
    registry: &'a Registry,
    selector: &Selector,
    sticky_machine: &str,
    sticky_workspace: &str,
    busy: &BTreeMap<String, usize>,
    load: &BTreeMap<String, probe::Assessment>,
) -> Result<Placement<'a>, MatchError> {
    let fits = |m: &&Machine| {
        selector.tags.iter().all(|tag| m.tags.contains(tag))
            && (selector.workspace.is_empty() || m.workspace(&selector.workspace).is_some())
            && (selector.backend.is_empty() || m.backends.contains(&selector.backend))
    };
    let machine = if !selector.machine.is_empty() {
        let m = registry.get(&selector.machine).ok_or_else(|| {
            error(
                format!("unknown machine {}", quoted(&selector.machine)),
                registry.names(),
            )
        })?;
        let missing: BTreeSet<_> = selector
            .tags
            .iter()
            .filter(|t| !m.tags.contains(t))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(error(
                format!(
                    "machine {} lacks {}",
                    m.name,
                    missing.into_iter().collect::<Vec<_>>().join(", ")
                ),
                registry
                    .machines
                    .iter()
                    .filter(fits)
                    .map(|m| m.name.clone())
                    .collect(),
            ));
        }
        m
    } else if !selector.tags.is_empty() {
        let candidates: Vec<_> = registry.machines.iter().filter(fits).collect();
        if candidates.is_empty() {
            return Err(error(
                format!(
                    "no machine has {}{}",
                    selector.tags.join(", "),
                    if selector.workspace.is_empty() {
                        String::new()
                    } else {
                        format!(" and workspace {}", quoted(&selector.workspace))
                    }
                ),
                registry.names(),
            ));
        }
        // Probed saturation only steers choices among matches; machines without
        // a reading count as available, so an empty `load` keeps the job-count rule.
        let saturated = |m: &&Machine| load.get(&m.name).is_some_and(|a| a.saturated);
        let preferred: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|m| (m.name == sticky_machine || m.name == registry.default) && !saturated(m))
            .collect();
        let open: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|m| !saturated(m))
            .collect();
        let eligible = if !preferred.is_empty() {
            &preferred
        } else if !open.is_empty() {
            &open
        } else {
            &candidates
        };
        let pressure = |m: &Machine| {
            let jobs = *busy.get(&m.name).unwrap_or(&0) as f64 / m.max_jobs as f64;
            jobs.max(load.get(&m.name).map_or(0., |a| a.score))
        };
        // Stable iteration breaks equal-load ties in configuration order.
        *eligible
            .iter()
            .min_by(|a, b| pressure(a).total_cmp(&pressure(b)))
            .unwrap()
    } else {
        let fallback = registry
            .get(sticky_machine)
            .or_else(|| registry.get(&registry.default))
            .ok_or_else(|| error("default machine is not configured".into(), registry.names()))?;
        if !selector.workspace.is_empty() && fallback.workspace(&selector.workspace).is_none() {
            let holders: Vec<_> = registry
                .machines
                .iter()
                .filter(|m| m.workspace(&selector.workspace).is_some())
                .collect();
            if holders.len() != 1 {
                return Err(error(
                    format!(
                        "workspace {} is {}",
                        quoted(&selector.workspace),
                        if holders.is_empty() {
                            "not configured"
                        } else {
                            "on several machines; name one"
                        }
                    ),
                    if holders.is_empty() {
                        registry.names()
                    } else {
                        holders.iter().map(|m| m.name.clone()).collect()
                    },
                ));
            }
            holders[0]
        } else {
            fallback
        }
    };
    let names = || machine.workspaces.iter().map(|w| w.name.clone()).collect();
    let workspace = if !selector.workspace.is_empty() {
        machine.workspace(&selector.workspace).ok_or_else(|| {
            error(
                format!(
                    "machine {} has no workspace {}",
                    machine.name,
                    quoted(&selector.workspace)
                ),
                names(),
            )
        })?
    } else if (machine.name == sticky_machine || sticky_machine.is_empty())
        && machine.workspace(sticky_workspace).is_some()
    {
        machine.workspace(sticky_workspace).unwrap()
    } else if machine.workspaces.len() == 1 {
        &machine.workspaces[0]
    } else {
        return Err(error(
            format!("machine {} has several workspaces; name one", machine.name),
            names(),
        ));
    };
    let backend = if selector.backend.is_empty() {
        &machine.default_backend
    } else {
        &selector.backend
    };
    if !machine.backends.contains(backend) {
        return Err(error(
            format!("machine {} has no {} backend", machine.name, backend),
            machine.backends.clone(),
        ));
    }
    Ok(Placement {
        machine,
        workspace,
        backend: backend.clone(),
    })
}
