//! The event feed: a stable, versioned projection of the replay ledger for
//! local tools that follow the daemon. `replay_events.seq` is the cursor, so
//! it survives restarts and migrations; records that mean nothing to a
//! watcher project to nothing while the cursor still moves past them.
use crate::{core::ids::ThreadId, slack::names::Names};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Map, Value};

/// The schema version of every object this feed emits. Fields are only added
/// within a version; a removal or rename bumps it.
pub const VERSION: u64 = 1;
/// Most ledger records one read scans.
pub const MAX_LIMIT: usize = 1000;

/// What a watcher needs to find the ledger row an event came from, and the
/// jobs and posts that only carry an ID in the ledger.
pub struct Lookup<'a> {
    pub names: &'a Names,
    /// Outbox post kind and thread for a delivery, by outbox ID.
    pub post: &'a dyn Fn(i64) -> Option<(String, String)>,
    /// The thread a job belongs to, by job ID.
    pub job: &'a dyn Fn(&str) -> Option<String>,
    /// Whether the `intake` record (event ID, seq, time) is a message's first
    /// arrival. Slack may deliver an envelope again; the ledger records each
    /// arrival, the feed one message.
    pub first: &'a dyn Fn(&str, i64, f64) -> bool,
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
fn place(
    names: &Names,
    workspace: &str,
    channel: &str,
    thread: Option<&str>,
) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("workspace".into(), json!(workspace));
    out.insert(
        "channel".into(),
        json!({"id":channel,"name":names.channels.get(channel)}),
    );
    out.insert("thread".into(), json!(thread));
    out
}
fn session(names: &Names, id: &str) -> Map<String, Value> {
    match id.parse::<ThreadId>() {
        Ok(t) => place(names, &t.workspace.0, &t.channel.0, Some(&t.root_ts.0)),
        Err(_) => place(names, "", "", None),
    }
}
fn clip(value: &Value, limit: usize) -> String {
    text(value).chars().take(limit).collect()
}

/// One event, or `None` when this record is not the feed's business.
pub fn project(seq: i64, time: f64, kind: &str, p: &Value, lookup: &Lookup<'_>) -> Option<Value> {
    let names = lookup.names;
    let (mut body, event) = match kind {
        "service_start" => (
            place(names, "", "", None),
            json!({"kind":"daemon","action":"started","observe_only":p["observe_only"]==true}),
        ),
        "service_stop" => (
            place(names, "", "", None),
            json!({"kind":"daemon","action":"stopped","failure":p["failure"]}),
        ),
        "intake" => {
            let m = &p["message"];
            if !(lookup.first)(text(&m["event_id"]), seq, time) {
                return None;
            }
            let ts = text(&m["ts"]);
            let root = m["thread_ts"].as_str().unwrap_or(ts);
            let sender = text(&m["sender"]);
            let mention = format!("<@{}>", text(&p["owner"]));
            let mut event = json!({
                "kind":"message","ts":ts,"sender":sender,
                "sender_name":names.users.get(sender),
                "mentions_owner":m["text"].as_str().is_some_and(|t| t.contains(&mention)),
                "text":m["text"],"files":m["attachments"].as_array().map_or(0, Vec::len),
                "source":m["source"],
            });
            // The owner's own posts carry Fridica's turn status.
            if !m["meta"].is_null() {
                event["turn_status"] = m["meta"]["status"].clone();
                event["turn_kind"] = m["meta"]["kind"].clone();
            }
            (
                place(
                    names,
                    text(&m["workspace"]),
                    text(&m["channel"]),
                    Some(root),
                ),
                event,
            )
        }
        "actor_commit" => {
            let d = &p["decision"];
            let reply = &d["reply"];
            let sent = reply["send"] != false && !text(&reply["text"]).trim().is_empty();
            let delegated = d["delegations"].as_array().is_some_and(|v| !v.is_empty());
            let status = text(&reply["status"]);
            // Independent atoms of what the turn did, sorted; empty when the
            // turn changed nothing visible. Clients test membership; atoms may
            // be added within a version.
            let mut outcome = vec![];
            if sent {
                outcome.push("replied");
            }
            if delegated {
                outcome.push("delegated");
            }
            if !reply.is_null() && matches!(status, "waiting" | "blocked") {
                outcome.push(status);
            }
            if reply["discussion"] == "finished" {
                outcome.push("finished");
            }
            outcome.sort_unstable();
            (
                session(names, text(&p["request"]["session"]["id"])),
                json!({
                    "kind":"turn","outcome":outcome,
                    "trigger_ts":p["request"]["trigger"]["message"]["ts"],
                    "delegations":d["delegations"].as_array().map_or(0, Vec::len),
                    "summary":clip(&d["summary"], 300),
                    "next_step":clip(&d["note"]["next_step"], 300),
                    "blocker":clip(&d["note"]["blocker"], 300),
                }),
            )
        }
        // The closing debrief is its own record: the finishing reply has
        // already been committed, and this turn posts nothing new itself.
        "debrief_commit" => (
            session(names, text(&p["request"]["session"]["id"])),
            json!({
                "kind":"turn","outcome":["finished"],
                "trigger_ts":p["request"]["trigger"]["message"]["ts"],
                "delegations":0,
                "summary":clip(&p["debrief"], 300),
                "next_step":"","blocker":"",
            }),
        ),
        "thread_control" => {
            let control = p["control"].as_object()?;
            let (name, detail) = control.iter().next()?;
            let action = match name.as_str() {
                "pause" => "paused",
                "resume" => "resumed",
                "close" => "closed",
                "archive" => "archived",
                "restore" => "restored",
                "clean" => "cleaned",
                "instruct" => "instructed",
                _ => return None,
            };
            (
                session(names, text(&p["session"])),
                json!({
                    "kind":"thread_control","action":action,
                    "reason":clip(&detail["reason"], 300),
                    "actor":p["authority"]["kind"],
                }),
            )
        }
        "delivery" => {
            let outcome = text(&p["result"]["outcome"]);
            if outcome == "sent" {
                return None;
            }
            let id = p["outbox_id"].as_i64().unwrap_or(-1);
            let (post_kind, thread) = (lookup.post)(id).unwrap_or_default();
            (
                session(names, &thread),
                json!({
                    "kind":"outbox","outcome":outcome,"code":p["result"]["code"],
                    "post_kind":post_kind,"outbox_id":id,"attempt":p["attempt"],
                }),
            )
        }
        "worker_call" => {
            let job = text(&p["request"]["job_id"]);
            let spec = &p["spec"];
            (
                session(names, &(lookup.job)(job).unwrap_or_default()),
                json!({
                    "kind":"job","action":"started","job_id":job,"attempt":p["request"]["attempt"],
                    "worker_id":spec["worker_id"],"machine":spec["machine"]["name"],
                    "workspace":spec["workspace"]["name"],"backend":spec["backend"],
                }),
            )
        }
        "worker_completion" => {
            let job = text(&p["job_id"]);
            let c = &p["completion"];
            let (action, status, code) = if c["interrupted"] == true {
                ("interrupted", Value::Null, Value::Null)
            } else if let Some(error) = c["outcome"].get("Err") {
                ("failed", Value::Null, error["code"].clone())
            } else {
                let result = &c["outcome"]["Ok"]["result"];
                (
                    if matches!(text(&result["status"]), "" | "done" | "complete") {
                        "finished"
                    } else {
                        "failed"
                    },
                    result["status"].clone(),
                    Value::Null,
                )
            };
            (
                session(names, &(lookup.job)(job).unwrap_or_default()),
                json!({
                    "kind":"job","action":action,"job_id":job,"attempt":p["attempt"],
                    "status":status,"code":code,
                }),
            )
        }
        _ => return None,
    };
    let mut out = Map::new();
    out.insert("v".into(), json!(VERSION));
    out.insert("cursor".into(), json!(seq));
    out.insert("time".into(), json!(time));
    out.insert("kind".into(), event["kind"].clone());
    out.append(&mut body);
    for (key, value) in event.as_object()? {
        if key != "kind" {
            out.insert(key.clone(), value.clone());
        }
    }
    Some(Value::Object(out))
}

/// The feed after `after`: at most `limit` ledger records are scanned, and
/// `next` is the last one scanned (or `after` when there was none), so a
/// client that resumes from `next` never sees a record twice.
pub fn read(c: &Connection, names: &Names, after: i64, limit: usize) -> Result<Value> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let rows: Vec<(i64, String, f64, String)> = c
        .prepare(
            "SELECT seq,kind,time,payload_json FROM replay_events WHERE seq>? ORDER BY seq LIMIT ?",
        )?
        .query_map(rusqlite::params![after, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let post = |id: i64| {
        c.query_row("SELECT kind,session_id FROM outbox WHERE id=?", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()
        .ok()
        .flatten()
    };
    let job = |id: &str| {
        c.query_row("SELECT session_id FROM jobs WHERE id=?", [id], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    };
    // The message row keeps its first arrival time; a repeat at the very same
    // time is found among the few records just before this one.
    let first = |event: &str, seq: i64, time: f64| {
        let stored: Option<f64> = c
            .query_row(
                "SELECT received_at FROM messages WHERE event_id=?",
                [event],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        match stored {
            None => true,
            Some(stored) if stored != time => false,
            Some(_) => !c
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM replay_events WHERE kind='intake' AND seq<?1 AND seq>=?1-1000 AND time=?2 AND json_extract(payload_json,'$.message.event_id')=?3)",
                    rusqlite::params![seq, time, event],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false),
        }
    };
    let lookup = Lookup {
        names,
        post: &post,
        job: &job,
        first: &first,
    };
    let mut next = after;
    let mut events = vec![];
    let scanned = rows.len();
    for (seq, kind, time, payload) in rows {
        next = seq;
        let Ok(payload) = serde_json::from_str::<Value>(&payload) else {
            continue;
        };
        if let Some(event) = project(seq, time, &kind, &payload, &lookup) {
            events.push(event);
        }
    }
    // A page that scanned fewer records than asked reached the ledger's end;
    // the feed's own control calls are records too, so `next` keeps moving.
    Ok(json!({"v":VERSION,"events":events,"next":next,"scanned":scanned}))
}
/// Where the ledger ends now: the cursor a new follower starts from.
pub fn end(c: &Connection) -> Result<i64> {
    Ok(
        c.query_row("SELECT COALESCE(MAX(seq),0) FROM replay_events", [], |r| {
            r.get(0)
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lookup(names: &Names) -> Lookup<'_> {
        fn post(id: i64) -> Option<(String, String)> {
            (id == 7).then(|| ("reply".to_string(), "TTEAM:CROOM:100.1".to_string()))
        }
        fn job(id: &str) -> Option<String> {
            (id == "job-0000000000000005").then(|| "TTEAM:CROOM:100.1".to_string())
        }
        // e1 first arrived as record 3; any later `intake` of it is a repeat.
        fn first(event: &str, seq: i64, _: f64) -> bool {
            event != "e1" || seq == 3
        }
        Lookup {
            names,
            post: &post,
            job: &job,
            first: &first,
        }
    }
    fn names() -> Names {
        Names {
            workspace: "TTEAM".into(),
            channels: [("CROOM".to_string(), "ai-human-plume".to_string())].into(),
            users: [("UALICE".to_string(), "Alice".to_string())].into(),
            ..Names::default()
        }
    }
    #[test]
    fn ledger_records_project_to_versioned_events_and_the_rest_to_nothing() {
        let n = names();
        let l = lookup(&n);
        // Payloads as recorded in tests/corpus/runtime.
        let intake = json!({"grace":900.0,"message":{"attachments":[],"channel":"CROOM","event_id":"e1","meta":null,"sender":"UALICE","source":"socket","text":"<@UOWNER> run checks","thread_ts":null,"ts":"100.1","workspace":"TTEAM"},"obligation_id":"obligation-0000000000000001","owner":"UOWNER"});
        let e = project(3, 10., "intake", &intake, &l).unwrap();
        assert_eq!(e["v"], 1);
        assert_eq!(e["cursor"], 3);
        assert_eq!(e["kind"], "message");
        assert_eq!(e["channel"], json!({"id":"CROOM","name":"ai-human-plume"}));
        assert_eq!(e["thread"], "100.1");
        assert_eq!(e["sender_name"], "Alice");
        assert_eq!(e["mentions_owner"], true);
        assert!(e.get("turn_status").is_none());
        // Slack delivered the same envelope again: recorded, not an event.
        assert!(project(16, 23., "intake", &intake, &l).is_none());
        let own = json!({"owner":"UOWNER","message":{"channel":"CROOM","ts":"100.2","thread_ts":"100.1","sender":"UOWNER","source":"socket","text":"On it.","meta":{"kind":"reply","status":"waiting"},"workspace":"TTEAM"}});
        let e = project(4, 11., "intake", &own, &l).unwrap();
        assert_eq!(
            (e["thread"].as_str(), e["turn_status"].as_str()),
            (Some("100.1"), Some("waiting"))
        );
        let commit = json!({"decision":{"delegations":[{"brief":"Run focused checks","machine":"local"}],"note":{"kind":"result","next_step":"","blocker":""},"reply":{"send":true,"status":"complete","text":"Running checks."},"summary":""},"request":{"session":{"id":"TTEAM:CROOM:100.1"},"trigger":{"kind":"message","message":{"ts":"100.1"}}}});
        let e = project(5, 12., "actor_commit", &commit, &l).unwrap();
        assert_eq!(e["kind"], "turn");
        assert_eq!(e["outcome"], json!(["delegated", "replied"]));
        assert_eq!(e["trigger_ts"], "100.1");
        let blocked = json!({"decision":{"delegations":[],"note":{"blocker":"needs a link"},"reply":{"send":true,"status":"blocked","text":"I need a link."},"summary":""},"request":{"session":{"id":"TTEAM:CROOM:100.1"},"trigger":{}}});
        assert_eq!(
            project(6, 13., "actor_commit", &blocked, &l).unwrap()["outcome"],
            json!(["blocked", "replied"])
        );
        let waiting = json!({"decision":{"delegations":[],"note":{},"reply":{"send":true,"status":"waiting","text":"Which file?"},"summary":""},"request":{"session":{"id":"TTEAM:CROOM:100.1"},"trigger":{}}});
        let e = project(7, 14., "actor_commit", &waiting, &l).unwrap();
        assert_eq!(e["outcome"], json!(["replied", "waiting"]));
        assert!(e.get("status").is_none());
        // A send=false stand-in only settles the status: no reply was posted.
        let settled = json!({"decision":{"delegations":[],"note":{"next_step":"owner review"},"reply":{"send":false,"status":"blocked","text":""},"summary":"handed to the owner"},"request":{"session":{"id":"TTEAM:CROOM:100.1"},"trigger":{}}});
        assert_eq!(
            project(17, 24., "actor_commit", &settled, &l).unwrap()["outcome"],
            json!(["blocked"])
        );
        // Nothing visible changed: the set is empty, not a made-up name.
        let nothing = json!({"decision":{"delegations":[],"note":{},"reply":null,"summary":""},"request":{"session":{"id":"TTEAM:CROOM:100.1"},"trigger":{}}});
        assert_eq!(
            project(18, 25., "actor_commit", &nothing, &l).unwrap()["outcome"],
            json!([])
        );
        let closing = json!({"decision":{"delegations":[],"note":{},"reply":{"send":true,"discussion":"finished","status":"complete","text":"All done."},"summary":""},"request":{"session":{"id":"TTEAM:CROOM:100.1"},"trigger":{}}});
        assert_eq!(
            project(19, 26., "actor_commit", &closing, &l).unwrap()["outcome"],
            json!(["finished", "replied"])
        );
        // The debrief that closes a discussion carries no reply at all; it is
        // its own ledger record and still reads as a finished turn.
        let debrief = json!({"inbox_id":9,"debrief":"Debrief: this discussion is finished.\n\nChecks passed.","request":{"call":"debrief","session":{"id":"TTEAM:CROOM:100.1"},"trigger":{"kind":"debrief"}}});
        let e = project(20, 27., "debrief_commit", &debrief, &l).unwrap();
        assert_eq!(e["kind"], "turn");
        assert_eq!(e["outcome"], json!(["finished"]));
        assert_eq!(e["thread"], "100.1");
        assert!(e["summary"].as_str().unwrap().starts_with("Debrief:"));
        assert!(e.get("status").is_none());
        let control = json!({"authority":{"kind":"owner"},"control":{"pause":{"reason":"Owner requests a review"}},"session":"TTEAM:CROOM:100.1"});
        let e = project(8, 15., "thread_control", &control, &l).unwrap();
        assert_eq!(
            (
                e["action"].as_str(),
                e["actor"].as_str(),
                e["reason"].as_str()
            ),
            (
                Some("paused"),
                Some("owner"),
                Some("Owner requests a review")
            )
        );
        let sent =
            json!({"attempt":1,"outbox_id":7,"result":{"outcome":"sent","reference":"200.1"}});
        assert!(project(9, 16., "delivery", &sent, &l).is_none());
        let refused = json!({"attempt":1,"outbox_id":7,"result":{"outcome":"rejected","code":"egress_ai_trailer"}});
        let e = project(10, 17., "delivery", &refused, &l).unwrap();
        assert_eq!(
            (
                e["kind"].as_str(),
                e["post_kind"].as_str(),
                e["code"].as_str(),
                e["thread"].as_str()
            ),
            (
                Some("outbox"),
                Some("reply"),
                Some("egress_ai_trailer"),
                Some("100.1")
            )
        );
        let call = json!({"request":{"attempt":1,"job_id":"job-0000000000000005"},"spec":{"backend":"codex","worker_id":"worker-0000000000000004","machine":{"name":"local"},"workspace":{"name":"project"}}});
        let e = project(11, 18., "worker_call", &call, &l).unwrap();
        assert_eq!(
            (
                e["kind"].as_str(),
                e["action"].as_str(),
                e["machine"].as_str(),
                e["thread"].as_str()
            ),
            (Some("job"), Some("started"), Some("local"), Some("100.1"))
        );
        let done = json!({"attempt":1,"job_id":"job-0000000000000005","completion":{"interrupted":false,"outcome":{"Ok":{"result":{"status":"done"}}}}});
        assert_eq!(
            project(12, 19., "worker_completion", &done, &l).unwrap()["action"],
            "finished"
        );
        let failed = json!({"attempt":1,"job_id":"job-0000000000000005","completion":{"interrupted":false,"outcome":{"Err":{"code":"files_download_failed"}}}});
        let e = project(13, 20., "worker_completion", &failed, &l).unwrap();
        assert_eq!(
            (e["action"].as_str(), e["code"].as_str()),
            (Some("failed"), Some("files_download_failed"))
        );
        assert!(project(14, 21., "parent_call", &json!({"call":"decide"}), &l).is_none());
        assert!(project(15, 22., "slack_http_call", &json!({}), &l).is_none());
    }
}
