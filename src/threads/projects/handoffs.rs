//! Address ready assignments in the project thread. The outbox insert and the
//! recipient marker commit together, including when a prerequisite clears.
use super::{Graph, ParentRequest, Project, Unit};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn plain(s: &str, limit: usize) -> String {
    s.chars()
        .take(limit)
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(super) fn queue(
    u: &mut dyn Unit,
    r: &ParentRequest,
    p: &mut Project,
    manager: &str,
    now: f64,
) -> Result<()> {
    // Legacy invalid graphs are shown for repair. They cannot dispatch work.
    let Ok(graph) = Graph::build(&p.assignments) else {
        return Ok(());
    };
    let ready: BTreeSet<String> = graph.ready.into_iter().map(str::to_owned).collect();
    let session = r.session["id"]
        .as_str()
        .context("missing project session")?;
    for (id, a) in &mut p.assignments {
        // Old free-text states can look ready by default. Require an explicit
        // ready report before delivering an assignment on the owner's behalf.
        if !ready.contains(id) || a["status"] != "ready" {
            a["handoff_to"] = Value::Null;
            continue;
        }
        let owner = a["owner"].as_str().unwrap_or("").trim();
        let member = owner
            .strip_prefix("<@")
            .and_then(|s| s.strip_suffix('>'))
            .unwrap_or(owner);
        if member == manager || !crate::config::loader::slack_id(member, "UW") {
            a["handoff_to"] = Value::Null;
            continue;
        }
        if a["handoff_to"] == member {
            continue;
        }
        let mut post = crate::threads::posts::post(
            format!("{}:project-assignment:{id}", r.inbox_id),
            session,
            "notice",
            r.session["channel"]
                .as_str()
                .context("missing project channel")?,
            Some(
                r.session["root_ts"]
                    .as_str()
                    .context("missing project root")?
                    .into(),
            ),
        );
        post.text = format!("<@{member}> please take assignment {}.\n{}\n{}\nPlease report the outcome in this thread.",
            plain(id,100), plain(&p.goal,250),plain(a["next_action"].as_str().unwrap_or(""),750));
        post.meta = Some(
            json!({"kind":"project_assignment","owner":manager,"session":session,"project":p.id,"v":2}),
        );
        u.queue_post(&post, now)?;
        a["handoff_to"] = json!(member);
    }
    Ok(())
}
