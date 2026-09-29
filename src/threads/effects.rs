//! Bounded parent edits to thread memory. No model field grants authority.
use crate::{
    config::Config,
    core::parent::{Decision, ParentRequest, ReplyStatus},
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

fn bounded(value: &mut String, limit: usize) -> Result<()> {
    *value = value.trim().to_owned();
    if value.chars().count() > limit {
        bail!("parent memory field exceeds {limit} characters");
    }
    Ok(())
}

pub(super) fn validate(decision: &mut Decision) -> Result<()> {
    bounded(&mut decision.summary, 2000)?;
    for value in [
        &mut decision.context.machine,
        &mut decision.context.workspace,
    ] {
        bounded(value, 64)?;
    }
    for value in [&mut decision.context.repo, &mut decision.context.branch] {
        bounded(value, 200)?;
    }
    for value in [
        &mut decision.note.repo,
        &mut decision.note.assignee,
        &mut decision.note.next_step,
        &mut decision.note.blocker,
    ] {
        bounded(value, 1000)?;
    }
    if decision.decisions.len() > 20 {
        bail!("at most 20 new decisions per turn");
    }
    for value in &mut decision.decisions {
        bounded(value, 500)?;
    }
    decision.decisions.retain(|s| !s.is_empty());
    Ok(())
}

pub(super) fn context(
    decision: &Decision,
    request: &ParentRequest,
    config: Option<&Config>,
) -> Result<Value> {
    let context = &decision.context;
    if !context.machine.is_empty() || !context.workspace.is_empty() {
        let registry = &config
            .context("context placement is not configured")?
            .machines;
        let target = if context.machine.is_empty() {
            request.session["context"]["machine"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(&registry.default)
        } else {
            &context.machine
        };
        let machine = registry.get(target).context("unknown context machine")?;
        if !context.workspace.is_empty() && machine.workspace(&context.workspace).is_none() {
            bail!("context workspace is not on the selected machine");
        }
        // A machine-only update must not retain an invalid workspace on the new
        // machine. The parent can repair the pair explicitly.
        let workspace = request.session["context"]["workspace"]
            .as_str()
            .unwrap_or("");
        if context.workspace.is_empty()
            && !workspace.is_empty()
            && machine.workspace(workspace).is_none()
        {
            bail!("context machine change needs a valid workspace");
        }
    }
    let mut patch = serde_json::to_value(context)?;
    patch
        .as_object_mut()
        .unwrap()
        .retain(|_, v| v.as_str().is_some_and(|s| !s.is_empty()));
    Ok(patch)
}

pub(super) fn notes(c: &Connection, session: &str) -> Result<(i64, Value)> {
    let row: Option<(i64, String)> = c.query_row("SELECT revision,data_json FROM notes WHERE session_id=? ORDER BY revision DESC LIMIT 1", [session], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    match row {
        Some((revision, raw)) => Ok((revision, serde_json::from_str(&raw)?)),
        None => Ok((0, json!({}))),
    }
}

pub(super) fn commit(
    c: &Connection,
    decision: &Decision,
    session: &str,
    inbox: i64,
    now: f64,
) -> Result<()> {
    let raw: String = c.query_row(
        "SELECT decisions_json FROM threads WHERE id=?",
        [session],
        |r| r.get(0),
    )?;
    let mut decisions: Vec<String> = serde_json::from_str(&raw)?;
    decisions.extend(decision.decisions.iter().cloned());
    if decisions.len() > 20 {
        decisions.drain(..decisions.len() - 20);
    }
    c.execute(
        "UPDATE threads SET decisions_json=? WHERE id=?",
        params![serde_json::to_string(&decisions)?, session],
    )?;

    let (revision, mut data) = notes(c, session)?;
    let original = data.clone();
    let fields = data
        .as_object_mut()
        .context("stored task note must be an object")?;
    let blocked = decision
        .reply
        .as_ref()
        .is_some_and(|r| matches!(r.status, ReplyStatus::Blocked));
    let posted: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM outbox WHERE session_id=? AND idem_key=?)",
        params![session, format!("{inbox}:reply")],
        |r| r.get(0),
    )?;
    if blocked && posted {
        for key in ["blocker", "assignee", "next_step"] {
            fields.insert(key.into(), json!(""));
        }
    }
    for (key, value) in [
        ("repo", &decision.note.repo),
        ("assignee", &decision.note.assignee),
        ("next_step", &decision.note.next_step),
        ("blocker", &decision.note.blocker),
    ] {
        if !value.is_empty() {
            fields.insert(key.into(), json!(value));
        }
    }
    if data != original {
        let revision = revision.checked_add(1).context("note revision overflow")?;
        c.execute("INSERT INTO notes(session_id,revision,actor,data_json,source,created) VALUES(?,?,'parent',?,?,?)",params![session,revision,data.to_string(),inbox.to_string(),now])?;
        c.execute("INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,'parent','notes.write',?,?)",params![now,session,json!({"revision":revision,"inbox_id":inbox}).to_string()])?;
    }
    Ok(())
}
