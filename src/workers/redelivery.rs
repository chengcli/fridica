//! A local delivery worker can return previously collected file bytes without
//! reopening the remote workspace or repeating the job that produced them.
use crate::core::{
    store::Unit,
    worker::{ArtifactRef, CollectedArtifact, Outcome},
};
use anyhow::Result;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(crate) fn startup_error(code: &str) -> Option<&'static str> {
    match code {
        "worker_github_mcp_unavailable" => {
            Some("The worker could not start its GitHub connection.")
        }
        "worker_isolation_runtime_or_transport_failed" => {
            Some("The worker could not start its isolated environment or connect to its host.")
        }
        _ => None,
    }
}

// An exact operation, so a job that asks to edit, split, or publish something
// cannot accidentally be replayed as a request for an older file.
pub(crate) fn requested(brief: &str) -> Option<(&str, &str)> {
    let (digest, name) = brief.strip_prefix("Resend saved file ")?.split_once('\n')?;
    (digest.len() == 64
        && digest.bytes().all(|b| b.is_ascii_hexdigit())
        && !name.is_empty()
        && !name.chars().any(char::is_control)
        && Path::new(name).file_name()?.to_str()? == name)
        .then_some((name, digest))
}

fn basename(path: &str) -> Option<&str> {
    Path::new(path).file_name()?.to_str()
}

/// Only inspect declared output of file jobs in this session. A report's
/// scratch files are never candidates for publication. Match bytes as well
/// as the name, since successive jobs often reuse the same path.
fn visit_saved(
    u: &mut dyn Unit,
    session: &str,
    wanted: impl Fn(&str) -> bool,
    mut visit: impl FnMut(ArtifactRef, Vec<u8>, &str) -> bool,
) -> Result<()> {
    for job in u.thread_jobs(session)?.into_iter().rev().filter(|j| {
        matches!(j.deliverable.as_str(), "markdown" | "figures_pdf") && j.result_json.is_some()
    }) {
        let result: Value = serde_json::from_str(job.result_json.as_deref().unwrap())?;
        let Some(refs) = result["artifacts"].as_array() else {
            continue;
        };
        if !refs
            .iter()
            .any(|r| r["path"].as_str().and_then(basename).is_some_and(&wanted))
        {
            continue;
        }
        for (path, data) in u.deliverable_files(session, &[Some(job.id)])? {
            let Some(name) = basename(&path).filter(|name| wanted(name)) else {
                continue;
            };
            if let Some(reference) = refs.iter().find(|r| r["path"] == path) {
                let mut reference: ArtifactRef = serde_json::from_value(reference.clone())?;
                reference.path = name.into();
                let digest = format!("{:x}", Sha256::digest(&data));
                if visit(reference, data, &digest) {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn saved(
    u: &mut dyn Unit,
    session: &str,
    name: &str,
    digest: &str,
) -> Result<Option<(ArtifactRef, Vec<u8>)>> {
    let mut found = None;
    visit_saved(
        u,
        session,
        |n| n == name,
        |reference, data, actual| {
            if !actual.eq_ignore_ascii_case(digest) {
                return false;
            }
            found = Some((reference, data));
            true
        },
    )?;
    Ok(found)
}

/// Names mentioned by the failed job are only candidates. Only an explicit
/// unchanged-file request selects a copy for delivery.
pub(crate) fn candidates(u: &mut dyn Unit, session: &str, briefs: &[String]) -> Result<Vec<Value>> {
    let mut files = Vec::new();
    visit_saved(
        u,
        session,
        |name| briefs.iter().any(|brief| brief.contains(name)),
        |reference, data, digest| {
            let file = json!({"name":reference.path,"bytes":data.len(),"sha256":digest});
            if !files.contains(&file) {
                files.push(file);
            }
            files.len() >= 12
        },
    )?;
    Ok(files)
}

pub(crate) fn outcome(
    reference: ArtifactRef,
    data: Vec<u8>,
    digest: &str,
) -> Result<(Outcome, Vec<CollectedArtifact>)> {
    let report = format!(
        "Verified {} ({} bytes, SHA-256 {}). Ready for upload.",
        reference.path,
        data.len(),
        digest
    );
    let result = serde_json::from_value(
        json!({"status":"done","summary":report,"report":report,"artifacts":[reference]}),
    )?;
    Ok((
        Outcome {
            result,
            backend_session_id: String::new(),
        },
        vec![CollectedArtifact {
            reference,
            data: Some(data),
            error: String::new(),
        }],
    ))
}
