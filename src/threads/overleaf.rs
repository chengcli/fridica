//! Resolve manuscript targets from the owner's messages in this Slack thread.
//! Parent summaries, worker output and channel history are never binding evidence.
use crate::core::parent::{Decision, ParentRequest, Reply, ReplyStatus};
use fridica_core::store::Unit;
use regex::Regex;
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::LazyLock};

static LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(?:(?P<origin>https?://[^\s/<>]+)/project/|",
        r"(?P<git>https://(?:git@)?git\.overleaf\.com/)|\boverleaf:)(?P<id>[0-9a-f]{24})\b"
    ))
    .expect("Overleaf project reference")
});
static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{24}\b").unwrap());

fn owner_projects(text: &str, origin: &str) -> BTreeSet<String> {
    let mut ids: BTreeSet<_> = LINK
        .captures_iter(text)
        .filter(|m| {
            m.name("origin").is_none_or(|url| {
                url.as_str()
                    .eq_ignore_ascii_case(origin.trim_end_matches('/'))
            }) && (m.name("git").is_none()
                || origin
                    .trim_end_matches('/')
                    .eq_ignore_ascii_case(crate::config::overleaf::DEFAULT_ORIGIN))
        })
        .map(|m| m["id"].to_ascii_lowercase())
        .collect();
    let text = text.trim().trim_start_matches("!f").trim();
    let lower = text.to_ascii_lowercase();
    let selection = lower
        .strip_prefix("confirm ")
        .or_else(|| lower.strip_prefix("use "))
        .unwrap_or(&lower);
    if ID.find(selection).is_some_and(|m| m.as_str() == selection) {
        ids.insert(selection.to_ascii_lowercase());
    } else if !text.contains("://") && text.to_ascii_lowercase().contains("overleaf") {
        ids.extend(ID.find_iter(text).map(|m| m.as_str().to_ascii_lowercase()));
    }
    ids
}

/// Read newest first. A deliberate selection settles earlier alternatives.
/// A passing mention of a second manuscript keeps both candidates visible.
fn binding<'a>(origin: &str, messages: impl Iterator<Item = (&'a str, &'a str)>) -> Option<Value> {
    let mut projects = BTreeSet::new();
    let mut latest = None;
    for (text, ts) in messages {
        let ids = owner_projects(text, origin);
        if ids.is_empty() {
            continue;
        }
        let body = text
            .trim()
            .trim_start_matches("!f")
            .trim()
            .to_ascii_lowercase();
        let selected = ids.len() == 1
            && (body.starts_with("use ")
                || body.starts_with("confirm ")
                || !body.contains(char::is_whitespace));
        projects.extend(ids);
        latest.get_or_insert(ts);
        if selected {
            break;
        }
    }
    latest.map(|ts| json!({"projects":projects,"message_ts":ts}))
}

pub(super) fn attach(
    u: &mut dyn Unit,
    request: &mut ParentRequest,
    owner: &str,
    origin: &str,
) -> anyhow::Result<()> {
    let reset = request.session["reset_at"].as_f64().unwrap_or(0.);
    let until = request.trigger["message"]["ts"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            request.trigger["ref"]
                .as_str()?
                .strip_prefix("slack-")?
                .replace('-', ".")
                .parse()
                .ok()
        })
        .unwrap_or(f64::INFINITY);
    let in_turn = |ts: &str| ts.parse::<f64>().is_ok_and(|v| v > reset && v <= until);
    // The parent sees a bounded history. Recover the original owner link
    // from durable messages even for long threads created before this guard.
    let evidence = if request.history.len() < 60 {
        binding(
            origin,
            request
                .history
                .iter()
                .rev()
                .filter(|m| m["sender"] == owner && m["meta"].is_null())
                .filter_map(|m| Some((m["text"].as_str()?, m["ts"].as_str()?)))
                .filter(|(_, ts)| in_turn(ts)),
        )
    } else {
        let messages = u.thread_messages(request.session["id"].as_str().unwrap_or(""), -1)?;
        binding(
            origin,
            messages
                .iter()
                .rev()
                .filter(|m| m.sender == owner && m.meta_json.is_none() && in_turn(&m.ts))
                .map(|m| (m.text.as_str(), m.ts.as_str())),
        )
    };
    if let Some(value) = evidence {
        request.session["overleaf_binding"] = value;
    }
    Ok(())
}

pub(super) fn check(decision: &mut Decision, request: &ParentRequest) -> Result<(), &'static str> {
    let bound = request.session["overleaf_binding"]["projects"]
        .as_array()
        .filter(|ids| ids.len() == 1)
        .and_then(|ids| ids[0].as_str());
    let repo = if decision.context.repo.is_empty() {
        request.session["context"]["repo"].as_str().unwrap_or("")
    } else {
        &decision.context.repo
    };
    for job in &mut decision.delegations {
        if crate::workers::redelivery::requested(&job.brief).is_some() {
            continue;
        }
        let header = crate::overleaf::worker::job_project(&job.brief);
        if header.is_some() && header != bound {
            return Err("Please confirm the Overleaf project link in this thread so I can bind the job to it.");
        }
        let project_repo = repo.starts_with("overleaf:") || LINK.is_match(repo);
        if !project_repo
            && !LINK.is_match(&job.brief)
            && !(repo.is_empty()
                && job.fetch_repo.is_empty()
                && job.brief.to_ascii_lowercase().contains("overleaf"))
        {
            continue;
        }
        // An account-navigation job can run before a target is selected. Its
        // native grant permits reads only; project writes still need this binding.
        if bound.is_none()
            && repo.is_empty()
            && job.fetch_repo.is_empty()
            && !LINK.is_match(&job.brief)
        {
            continue;
        }
        let id = bound.ok_or("Which Overleaf project should I use? Please send its project link or 24-character ID in this thread.")?;
        if [repo, &job.brief]
            .iter()
            .flat_map(|s| ID.find_iter(s))
            .any(|m| !m.as_str().eq_ignore_ascii_case(id))
        {
            return Err("The proposed job refers to a different Overleaf project. Please confirm the target with its project link in this thread.");
        }
        if header.is_none() {
            job.brief = crate::overleaf::worker::project_brief(id, &job.brief);
        }
    }
    Ok(())
}

pub(super) fn clarify(text: &str) -> Decision {
    Decision {
        reply: Some(Reply {
            send: true,
            status: ReplyStatus::Waiting,
            text: text.into(),
            discussion: Default::default(),
            details: String::new(),
            answers: vec![],
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multiple_manuscripts_remain_ambiguous_until_selected() {
        let a = "111111111111111111111111";
        let b = "222222222222222222222222";
        let first = format!("!f Check https://www.overleaf.com/project/{a}");
        let other = format!("Compare with https://www.overleaf.com/project/{b}");
        let messages = [(other.as_str(), "2"), (first.as_str(), "1")];
        assert_eq!(
            binding(
                crate::config::overleaf::DEFAULT_ORIGIN,
                messages.into_iter()
            )
            .unwrap()["projects"],
            json!([a, b])
        );
        assert_eq!(
            owner_projects(
                &format!("https://papers.example.invalid/project/{a}"),
                "https://papers.example.invalid"
            ),
            BTreeSet::from([a.to_owned()])
        );
        assert!(owner_projects(
            &format!("https://papers.example.invalid/project/{a}"),
            crate::config::overleaf::DEFAULT_ORIGIN
        )
        .is_empty());
        let confirm = format!("!f use {b}");
        let selected = std::iter::once((confirm.as_str(), "3")).chain(messages);
        assert_eq!(
            binding(crate::config::overleaf::DEFAULT_ORIGIN, selected),
            Some(json!({"projects":[b],"message_ts":"3"}))
        );
    }
}
