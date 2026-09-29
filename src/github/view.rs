//! Compact projections of GitHub responses; PR bodies never establish facts.
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;

static STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:[-*•]\s*)?[*_`]*\s*(owner|next action|next|blocker|waiting[- ]on|head|tree)\s*[*_`]*\s*:\s*[*_`]*\s*(.+?)\s*[*_`]*\s*$").unwrap()
});
static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?(?:-->|$)").unwrap());
static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[0-9a-fA-F]{7,40}").unwrap());
static TREE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\btree\b[^0-9a-fA-F]{0,8}([0-9a-fA-F]{7,40})").unwrap());
const ORDER: [&str; 4] = ["owner", "next", "blocker", "waiting on"];
pub fn text(v: &Value, limit: usize) -> String {
    v.as_str().unwrap_or("").chars().take(limit).collect()
}
fn short(v: &str) -> String {
    v.chars().take(7).collect()
}
fn boundary(value: &str, at: usize) -> bool {
    value
        .as_bytes()
        .get(at)
        .is_none_or(|v| !v.is_ascii_alphanumeric())
}
pub fn claim_sha(value: &str) -> Option<String> {
    HEX.find_iter(value)
        .find(|m| {
            (m.start() == 0 || !value.as_bytes()[m.start() - 1].is_ascii_alphanumeric())
                && boundary(value, m.end())
        })
        .map(|m| m.as_str().to_ascii_lowercase())
}
pub fn status_lines(body: &Value) -> (Map<String, Value>, Map<String, Value>) {
    let mut found = Map::new();
    let clean = COMMENT.replace_all(body.as_str().unwrap_or(""), "");
    for line in clean.lines().filter(|l| !l.trim().is_empty()).take(20) {
        let Some(m) = STATUS.captures(line) else {
            continue;
        };
        let key = m[1].to_ascii_lowercase().replace('-', " ");
        let key = if key == "next action" { "next" } else { &key };
        let value: String = m[2].chars().take(200).collect();
        found.entry(key).or_insert_with(|| json!(value));
        if key == "head" {
            if let Some(tree) = TREE
                .captures_iter(&value)
                .find(|c| boundary(&value, c.get(1).unwrap().end()))
            {
                found.entry("tree").or_insert_with(|| json!(&tree[1]));
            }
        }
    }
    (
        ORDER
            .into_iter()
            .filter_map(|k| found.get(k).map(|v| (k.into(), v.clone())))
            .collect(),
        ["head", "tree"]
            .into_iter()
            .filter_map(|k| found.get(k).map(|v| (k.into(), v.clone())))
            .collect(),
    )
}
pub fn checks(runs: &[Value]) -> (String, Value) {
    let mut counts = json!({"success":0,"failure":0,"cancelled":0,"pending":0,"other":0});
    for run in runs.iter().filter(|v| v.is_object()) {
        let kind = if run["status"] != "completed" {
            "pending"
        } else {
            match run["conclusion"].as_str().unwrap_or("") {
                "success" => "success",
                "failure" | "timed_out" | "action_required" | "startup_failure" => "failure",
                "cancelled" => "cancelled",
                _ => "other",
            }
        };
        counts[kind] = json!(counts[kind].as_u64().unwrap() + 1);
    }
    let mut overall = if counts["success"].as_u64().unwrap() > 0 {
        "success"
    } else if counts["other"].as_u64().unwrap() > 0 {
        "neutral"
    } else {
        "none"
    };
    for k in ["failure", "cancelled", "pending"] {
        if counts[k].as_u64().unwrap() > 0 {
            overall = k;
            break;
        }
    }
    (overall.into(), counts)
}
pub fn reviews(reviews: &[Value], head: &str) -> (Vec<String>, usize, usize, usize) {
    let mut latest: Vec<(String, &Value)> = vec![];
    for r in reviews {
        if !matches!(
            r["state"].as_str(),
            Some("APPROVED" | "CHANGES_REQUESTED" | "DISMISSED")
        ) {
            continue;
        }
        let login = r["user"]["login"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "unknown-{}",
                    r.get("id")
                        .map(Value::to_string)
                        .unwrap_or_else(|| latest.len().to_string())
                )
            });
        if let Some((_, old)) = latest.iter_mut().find(|(l, _)| *l == login) {
            *old = r;
        } else {
            latest.push((login, r));
        }
    }
    let (mut lines, mut current, mut stale, mut decisive) = (vec![], 0, 0, 0);
    for (login, r) in latest {
        let commit = r["commit_id"].as_str().unwrap_or("");
        match r["state"].as_str().unwrap() {
            "APPROVED" => {
                decisive += 1;
                if commit == head {
                    current += 1;
                    lines.push(format!("approved @{} by {login}", short(commit)));
                } else {
                    stale += 1;
                    lines.push(format!(
                        "approved @{} by {login} (stale: not the head)",
                        short(commit)
                    ));
                }
            }
            "CHANGES_REQUESTED" => {
                decisive += 1;
                lines.push(format!("changes requested @{} by {login}", short(commit)));
            }
            _ => lines.push(format!("dismissed @{} by {login}", short(commit))),
        }
    }
    (lines, current, stale, decisive)
}
fn names(value: &Value, field: &str) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v.is_object())
        .map(|v| v[field].as_str().unwrap_or("?").chars().take(200).collect())
        .collect()
}
fn status_parts(status: &Map<String, Value>) -> Vec<String> {
    ORDER
        .iter()
        .filter_map(|k| {
            status.get(*k).map(|v| {
                format!(
                    "{k}{}{}",
                    if *k == "owner" { " " } else { ": " },
                    v.as_str().unwrap()
                )
            })
        })
        .collect()
}
fn people(item: &Value) -> Vec<String> {
    ["assignees", "labels"]
        .into_iter()
        .filter_map(|k| {
            let names = item[k]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>();
            (!names.is_empty()).then(|| format!("{k} {}", names.join(", ")))
        })
        .collect()
}
pub fn issue(repo: &str, value: &Value) -> Value {
    let number = &value["number"];
    let (status, _) = status_lines(&value["body"]);
    let title = text(&value["title"], 200);
    let mut state = text(&value["state"], 20);
    if state.is_empty() {
        state = "?".into();
    }
    let mut item = json!({"link":format!("https://github.com/{repo}/issues/{number}"),"kind":"issue","repo":repo,"number":number,"title":title,"state":state,"assignees":names(&value["assignees"],"login"),"labels":names(&value["labels"],"name"),"status_block":status});
    let mut parts = vec![format!("issue {repo}#{number} \"{title}\""), state];
    parts.extend(status_parts(&status));
    parts.extend(people(&item));
    item["summary"] = json!(parts.join(" · "));
    item
}
pub type Pages = (Vec<Value>, bool);
pub struct Aux {
    pub tree: Option<String>,
    pub behind: Option<i64>,
    pub runs: Option<Pages>,
    pub reviews: Option<Pages>,
}
pub fn pull(repo: &str, value: &Value, aux: Aux) -> Value {
    let number = &value["number"];
    let head = value["head"]["sha"].as_str().unwrap_or("");
    let base = value["base"]["ref"].as_str().unwrap_or("");
    let title = text(&value["title"], 200);
    let mut state = if value["merged"] == true {
        "merged".into()
    } else {
        text(&value["state"], 20)
    };
    if state.is_empty() {
        state = "?".into();
    }
    if state == "open" && value["draft"] == true {
        state = "draft".into();
    }
    let (mut ci, counts) = aux
        .runs
        .as_ref()
        .map(|(runs, _)| checks(runs))
        .unwrap_or_else(|| ("unavailable".into(), json!({})));
    if aux.runs.as_ref().is_some_and(|(_, complete)| !complete)
        && matches!(ci.as_str(), "success" | "neutral" | "none")
    {
        ci = "incomplete".into();
    }
    let (review_lines, current, stale, decisive) = reviews(
        aux.reviews.as_ref().map(|r| r.0.as_slice()).unwrap_or(&[]),
        head,
    );
    let (status, claims) = status_lines(&value["body"]);
    let mut mergeable = text(&value["mergeable_state"], 20);
    if mergeable.is_empty() {
        mergeable = "unknown".into();
    }
    let tree = aux.tree.as_deref().filter(|v| !v.is_empty());
    let mut item = json!({"link":format!("https://github.com/{repo}/pull/{number}"),"kind":"pull","repo":repo,"number":number,"title":title,"state":state,"head":head,"tree":tree.unwrap_or("unavailable"),"base":base,"behind_base":aux.behind.map(|v|v>0),"mergeable":mergeable,"ci":ci,"checks":counts,"reviews":if aux.reviews.is_some(){json!(review_lines)}else{json!("unavailable")},"assignees":names(&value["assignees"],"login"),"labels":names(&value["labels"],"name"),"status_block":status});
    let mut parts = vec![
        format!("PR {repo}#{number} \"{title}\""),
        state,
        format!("head {}", short(head)),
        format!("CI {ci}"),
        if aux.reviews.is_some() {
            format!("approvals {current}/{decisive}")
        } else {
            "reviews unavailable".into()
        },
    ];
    parts.extend(status_parts(&status));
    parts.extend([
        format!("tree {}", tree.map(short).unwrap_or("unavailable".into())),
        format!(
            "base {base}{}",
            aux.behind
                .map(|n| if n > 0 {
                    " (behind base)"
                } else {
                    " (up to date)"
                })
                .unwrap_or("")
        ),
        format!("mergeable {mergeable}"),
    ]);
    if counts["cancelled"].as_u64().unwrap_or(0) > 0 {
        parts.push(format!("cancelled checks {}", counts["cancelled"]));
    }
    if stale > 0 {
        parts.push(format!("stale approvals {stale}"));
    }
    if aux.reviews.as_ref().is_some_and(|(_, complete)| !complete) {
        parts.push("reviews truncated".into());
    }
    let mut verdicts = Map::new();
    for (key, actual) in [("head", Some(head)), ("tree", tree)] {
        if let Some(claim) = claims.get(key) {
            let sha = claim_sha(claim.as_str().unwrap());
            let verdict = match (actual, sha) {
                (None, _) => "unverified (the real value could not be fetched)",
                (_, None) => "is not a commit id",
                (Some(actual), Some(sha)) if actual.to_ascii_lowercase().starts_with(&sha) => {
                    "matches"
                }
                _ => "does NOT match",
            };
            verdicts.insert(key.into(), json!(verdict));
            parts.push(format!("body's {key} claim {verdict}"));
        }
    }
    if !verdicts.is_empty() {
        item["claims"] = json!(verdicts);
    }
    parts.extend(people(&item));
    item["summary"] = json!(parts.join(" · "));
    item
}
