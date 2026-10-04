//! Terminal log for `fridica start`. Follows the recorded event log and prints one
//! line per notable event to stderr, in v0.3's `time LEVEL name: message` form.
//! It only reads; it never prints message text, briefs, tokens or paths.
//! `FRIDICA_LOG=off` silences it.
use crate::{
    config::schema::Slack,
    slack::names::{self, Names, UserNames},
    store::Store,
};
use fridica_core::store::{Event, Store as _, Unit};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;

pub fn enabled() -> bool {
    std::env::var("FRIDICA_LOG").map_or(true, |v| !matches!(v.as_str(), "off" | "0" | "false"))
}
pub fn line(level: &str, name: &str, message: &str) {
    if enabled() {
        eprintln!(
            "{} {level} {name}: {message}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
    }
}

/// Print events recorded after the call until `finished` turns true, then drain.
/// With `lookup`, senders are shown by name: each unknown ID is looked up
/// once (again after ten minutes if that failed) and the name is recorded.
pub async fn follow(
    store: Store,
    slack: Slack,
    lookup: Option<Arc<dyn UserNames>>,
    mut finished: watch::Receiver<bool>,
) {
    if !enabled() {
        return;
    }
    let mut attempted: BTreeMap<String, std::time::Instant> = BTreeMap::new();
    let mut seq: i64 = store.transact(|u| u.last_seq()).await.unwrap_or(0);
    loop {
        let done = *finished.borrow();
        let after = seq;
        let scope = slack.clone();
        let names: Names = store
            .transact(move |u| Names::recorded(u, &scope))
            .await
            .unwrap_or_default();
        if let Some(lookup) = &lookup {
            let unknown: Vec<String> = store
                .transact(move |u| unknown_senders(u, after))
                .await
                .unwrap_or_default();
            let retry = Duration::from_secs(600);
            for sender in unknown.into_iter().filter(|s| !names.users.contains_key(s)) {
                if attempted
                    .get(&sender)
                    .is_some_and(|at| at.elapsed() < retry)
                {
                    continue;
                }
                attempted.insert(sender.clone(), std::time::Instant::now());
                if let Some(name) = lookup.user_name(&sender).await {
                    let (id, recorded) = (sender.clone(), name.clone());
                    let _ = store
                        .transact(move |u| names::record_user(u, &id, &recorded))
                        .await;
                }
            }
            attempted.retain(|_, at| at.elapsed() < retry);
        }
        let slack = slack.clone();
        if let Ok((last, lines)) = store.transact(move |u| read(u, &slack, after)).await {
            seq = last;
            for (level, name, message) in lines {
                line(level, name, &message);
            }
        }
        if done {
            return;
        }
        tokio::select! {
            _ = finished.changed() => {}
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

type Line = (&'static str, &'static str, String);

/// Senders of messages taken in after `after`, at most a handful per pass.
fn unknown_senders(u: &mut dyn Unit, after: i64) -> anyhow::Result<Vec<String>> {
    let rows = u.intake_senders_after(after)?;
    let mut seen = BTreeSet::new();
    Ok(rows
        .into_iter()
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .take(5)
        .collect())
}

fn read(u: &mut dyn Unit, slack: &Slack, after: i64) -> anyhow::Result<(i64, Vec<Line>)> {
    let names = Names::recorded(u, slack)?;
    let rows = u.events_after(after, 500)?;
    let u = RefCell::new(u);
    let mut last = after;
    let mut lines = vec![];
    for Event {
        seq, kind, payload, ..
    } in rows
    {
        last = seq;
        let Ok(payload) = serde_json::from_str::<Value>(&payload) else {
            continue;
        };
        let post = |id: i64| {
            u.borrow_mut()
                .outbox_post(id)
                .ok()
                .flatten()
                .map(|p| (p.kind, p.session))
        };
        if let Some(line) = describe(&post, &names, &kind, &payload) {
            lines.push(line);
        }
    }
    Ok((last, lines))
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
fn short(id: &str) -> &str {
    let tail = id.rsplit('-').next().unwrap_or(id);
    if tail.len() >= 6 {
        &tail[..6]
    } else {
        id
    }
}
/// Outbox post kind and thread for a delivery, looked up by outbox ID.
type Post<'a> = &'a dyn Fn(i64) -> Option<(String, String)>;

pub(crate) fn describe(post: Post<'_>, names: &Names, kind: &str, p: &Value) -> Option<Line> {
    Some(match kind {
        "service_start" => (
            "INFO",
            "fridica",
            if p["observe_only"] == true {
                "running in observe-only mode (no replies or jobs)".into()
            } else {
                "running".into()
            },
        ),
        "service_stop" => match &p["failure"] {
            Value::Null => ("INFO", "fridica", "stopped".into()),
            failure => ("WARNING", "fridica", format!("stopped: {failure}")),
        },
        "slack_socket_state" => match &p["failure"] {
            Value::Null => ("INFO", "slack", text(&p["status"]).to_string()),
            failure => (
                "WARNING",
                "slack",
                format!("{} ({failure})", text(&p["status"])),
            ),
        },
        "service_catchup" => {
            let incomplete = p["incomplete_channels"].as_array().map_or(0, Vec::len);
            if incomplete > 0 {
                (
                    "WARNING",
                    "slack",
                    format!("catch-up incomplete in {incomplete} channel(s)"),
                )
            } else if p["added"].as_u64().unwrap_or(0) > 0 {
                (
                    "INFO",
                    "slack",
                    format!("caught up on {} missed message(s)", p["added"]),
                )
            } else {
                return None;
            }
        }
        "intake" => {
            let m = &p["message"];
            // Our own posts come back as events; they are not news.
            if !m["meta"].is_null() && m["sender"] == p["owner"] {
                return None;
            }
            let root = m["thread_ts"].as_str().unwrap_or(text(&m["ts"]));
            let source = if m["source"] == "catchup" {
                " (catch-up)"
            } else {
                ""
            };
            (
                "INFO",
                "intake",
                format!(
                    "{}:{root} from {}{source}",
                    names.channel(text(&m["channel"])),
                    names.user(text(&m["sender"]))
                ),
            )
        }
        "actor_commit" => {
            let d = &p["decision"];
            let mut parts = vec![];
            if d["reply"]["send"] != false && !text(&d["reply"]["text"]).is_empty() {
                parts.push(format!("replied ({})", text(&d["reply"]["status"])));
            }
            if let Some(delegations) = d["delegations"].as_array().filter(|d| !d.is_empty()) {
                let places: Vec<String> = delegations
                    .iter()
                    .map(|d| match text(&d["machine"]) {
                        "" => d["tags"].as_array().map_or("default".into(), |t| {
                            t.iter().map(text).collect::<Vec<_>>().join("+")
                        }),
                        machine => machine.to_string(),
                    })
                    .collect();
                parts.push(format!(
                    "delegated {} job(s) to {}",
                    delegations.len(),
                    places.join(", ")
                ));
            }
            if parts.is_empty() {
                parts.push("no reply".into());
            }
            (
                "INFO",
                "parent",
                format!(
                    "{}: {}",
                    names.thread(text(&p["request"]["session"]["id"])),
                    parts.join("; ")
                ),
            )
        }
        "parent_result" => match (&p["failure"], &p["error"]) {
            (Value::Null, Value::Null) => return None,
            (Value::Null, error) => ("WARNING", "parent", format!("call failed: {error}")),
            (failure, _) => ("WARNING", "parent", format!("call failed: {failure}")),
        },
        "worker_call" => {
            let spec = &p["spec"];
            (
                "INFO",
                "worker",
                format!(
                    "job {} started on {} ({}, slot {})",
                    short(text(&p["request"]["job_id"])),
                    text(&spec["machine"]["name"]),
                    text(&spec["backend"]),
                    spec["slot"]
                ),
            )
        }
        "worker_completion" => {
            let job = short(text(&p["job_id"]));
            let completion = &p["completion"];
            if completion["interrupted"] == true {
                ("WARNING", "worker", format!("job {job} interrupted"))
            } else if let Some(error) = completion["outcome"].get("Err") {
                (
                    "WARNING",
                    "worker",
                    format!("job {job} failed ({})", text(&error["code"])),
                )
            } else {
                match text(&completion["outcome"]["Ok"]["result"]["status"]) {
                    "" | "complete" | "done" => ("INFO", "worker", format!("job {job} finished")),
                    status => ("WARNING", "worker", format!("job {job} finished: {status}")),
                }
            }
        }
        "delivery" => {
            let found = post(p["outbox_id"].as_i64().unwrap_or(-1));
            let (what, session) = found.unwrap_or_else(|| ("post".into(), String::new()));
            let place = names.thread(&session);
            match text(&p["result"]["outcome"]) {
                "sent" => ("INFO", "slack", format!("posted {what} in {place}")),
                outcome => (
                    "WARNING",
                    "slack",
                    format!(
                        "{what} in {place} {outcome}{}",
                        match text(&p["result"]["code"]) {
                            "" => String::new(),
                            code => format!(" ({code})"),
                        }
                    ),
                ),
            }
        }
        "control_request" => (
            "INFO",
            "control",
            format!("{} {}", text(&p["method"]), text(&p["target"])),
        ),
        "machine_load_result" if p["reading"].is_null() => (
            "WARNING",
            "placement",
            format!(
                "could not probe {}; treating it as available",
                text(&p["machine"])
            ),
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn describes_notable_events_without_message_text() {
        let post = |id: i64| (id == 7).then(|| ("reply".to_string(), "T:C1:100.1".to_string()));
        let names = Names {
            workspace: "T".into(),
            channels: [("C1".to_string(), "ai-human-plume".to_string())].into(),
            users: [("UA".to_string(), "Ada".to_string())].into(),
            ..Names::default()
        };
        let say = |kind: &str, p: Value| {
            describe(&post, &names, kind, &p).map(|(l, n, m)| format!("{l} {n}: {m}"))
        };
        assert_eq!(
            say("service_start", json!({"observe_only":false})).unwrap(),
            "INFO fridica: running"
        );
        let intake = json!({"owner":"UO","message":{"channel":"C1","ts":"100.1","thread_ts":null,"sender":"UA","source":"socket","text":"private words","meta":null}});
        let line = say("intake", intake).unwrap();
        assert_eq!(line, "INFO intake: #ai-human-plume:100.1 from @Ada");
        let other = json!({"owner":"UO","message":{"channel":"C1","ts":"100.2","thread_ts":null,"sender":"UB","source":"socket","text":"x","meta":null}});
        assert_eq!(
            say("intake", other).unwrap(),
            "INFO intake: #ai-human-plume:100.2 from UB"
        );
        assert!(say("intake", json!({"owner":"UO","message":{"channel":"C1","ts":"1","sender":"UO","meta":{"kind":"reply"}}})).is_none());
        let commit = json!({"request":{"session":{"id":"T:C1:100.1"}},"decision":{"reply":{"send":true,"text":"secret reply","status":"complete"},"delegations":[{"machine":"dart10"},{"machine":"","tags":["cuda"]}]}});
        assert_eq!(say("actor_commit", commit).unwrap(), "INFO parent: #ai-human-plume:100.1: replied (complete); delegated 2 job(s) to dart10, cuda");
        assert_eq!(
            say("worker_call", json!({"request":{"job_id":"job-c5ebde2a-7a39-4a0e-af86-1eb487ec49ec"},"spec":{"machine":{"name":"dart11"},"backend":"codex","slot":1}})).unwrap(),
            "INFO worker: job 1eb487 started on dart11 (codex, slot 1)"
        );
        assert_eq!(
            say("worker_completion", json!({"job_id":"job-a-b","completion":{"interrupted":false,"outcome":{"Ok":{"result":{"status":"failed"}}}}})).unwrap(),
            "WARNING worker: job job-a-b finished: failed"
        );
        assert_eq!(
            say(
                "delivery",
                json!({"outbox_id":7,"result":{"outcome":"sent"}})
            )
            .unwrap(),
            "INFO slack: posted reply in #ai-human-plume:100.1"
        );
        assert_eq!(
            say(
                "delivery",
                json!({"outbox_id":7,"result":{"outcome":"rejected","code":"invalid_arguments"}})
            )
            .unwrap(),
            "WARNING slack: reply in #ai-human-plume:100.1 rejected (invalid_arguments)"
        );
        assert_eq!(
            say(
                "control_request",
                json!({"method":"POST","target":"/channels/C1/instruct","body":{"text":"secret"}})
            )
            .unwrap(),
            "INFO control: POST /channels/C1/instruct"
        );
        assert!(say(
            "machine_load_result",
            json!({"machine":"dart10","reading":{"load1":1}})
        )
        .is_none());
        assert!(say("backend_wire", json!({})).is_none());
        for kind in ["intake", "actor_commit", "control_request"] {
            let all = [say(
                kind,
                json!({"owner":"UO","message":{"text":"private words"},"request":{"session":{"id":"x"}},"decision":{},"body":{"text":"secret"}}),
            )];
            assert!(all
                .iter()
                .flatten()
                .all(|l| !l.contains("private") && !l.contains("secret")));
        }
    }
}
