//! Frozen v0.3 gate for differential replay. v0.4's attention gate is separate so
//! changes in policy are explicit rather than hidden as parity "normalization".
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct GateInput {
    pub owner: String,
    pub text: String,
    pub sender: String,
    pub generated: bool,
    pub meta_kind: String,
    pub meta_status: String,
    pub peer_turn: u64,
    pub control: String,
    pub status: String,
    pub turns: u64,
    pub reset_at: f64,
    pub ts: f64,
    pub general_messages: bool,
    pub cooling: bool,
    pub observe_only: bool,
    pub resumed: bool,
}
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub kind: String,
    pub reason: String,
    pub turn: u64,
}
impl Verdict {
    fn new(kind: &str, reason: &str, turn: u64) -> Self {
        Self {
            kind: kind.into(),
            reason: reason.into(),
            turn,
        }
    }
}
pub fn legacy_gate(i: &GateInput) -> Verdict {
    if i.observe_only {
        return Verdict::new("observe", "observe-only mode", 0);
    }
    if i.control != "active" {
        return Verdict::new("observe", &format!("thread is {}", i.control), 0);
    }
    if !i.resumed && i.reset_at != 0. && i.ts <= i.reset_at {
        return Verdict::new("observe", "sent before the thread was resumed", 0);
    }
    if i.sender == i.owner {
        return if i.generated {
            Verdict::new("ignore", "our own post", 0)
        } else {
            Verdict::new("observe", "the owner wrote", 0)
        };
    }
    let mentioned = i.text.contains(&format!("<@{}>", i.owner)) || i.resumed;
    let waiting = i.status == "waiting";
    let turn = i
        .turns
        .max(if i.generated { i.peer_turn } else { 0 })
        .saturating_add(1);
    if i.generated {
        if i.meta_kind == "debrief_root" {
            return Verdict::new("ignore", "another agent's debrief", 0);
        }
        if matches!(i.meta_status.as_str(), "complete" | "blocked") && !(mentioned || waiting) {
            return Verdict::new("ignore", "another agent finished its reply", 0);
        }
        if !(mentioned || waiting) {
            return Verdict::new("ignore", "another agent's message not addressed to us", 0);
        }
    }
    if i.status == "blocked" {
        return if mentioned {
            Verdict::new("notice", "blocked thread", turn)
        } else {
            Verdict::new("observe", "blocked thread", 0)
        };
    }
    if mentioned || waiting {
        return Verdict::new(
            "respond",
            if mentioned {
                "addressed"
            } else {
                "answering our question"
            },
            turn,
        );
    }
    if i.generated {
        return Verdict::new("ignore", "another agent's message", 0);
    }
    if i.turns > 0 {
        return Verdict::new("triage", "follow-up in a thread we are part of", turn);
    }
    if i.general_messages && !i.cooling {
        return Verdict::new("triage", "unaddressed message", turn);
    }
    Verdict::new("observe", "not addressed", 0)
}

pub fn attention_gate(i: &GateInput, new_instruction: bool) -> Verdict {
    let previous = legacy_gate(i);
    // v0.4: another agent's message that addresses nobody may still be a
    // factual ask; triage decides (the contract limits it to factual,
    // verifiable questions). Finished replies and debriefs stay ignored.
    let addressed =
        i.text.contains(&format!("<@{}>", i.owner)) || i.resumed || i.status == "waiting";
    if previous.kind == "ignore"
        && i.generated
        && !addressed
        && i.sender != i.owner
        && i.meta_kind != "debrief_root"
        && !matches!(i.meta_status.as_str(), "complete" | "blocked")
        && i.control == "active"
        && !i.observe_only
        && (i.resumed || i.reset_at == 0. || i.ts > i.reset_at)
    {
        return if i.status == "blocked" || i.cooling {
            Verdict::new("observe", "another agent's message not addressed to us", 0)
        } else {
            Verdict::new(
                "triage",
                "another agent's message: answer only a factual ask",
                i.turns.max(i.peer_turn).saturating_add(1),
            )
        };
    }
    if i.status == "blocked"
        && new_instruction
        && matches!(previous.kind.as_str(), "notice" | "observe")
        && i.control == "active"
        && !i.observe_only
        && i.sender != i.owner
        && (i.resumed || i.reset_at == 0. || i.ts > i.reset_at)
    {
        Verdict::new(
            "respond",
            "new instruction reopens blocked discussion",
            i.turns.max(i.peer_turn).saturating_add(1),
        )
    } else {
        previous
    }
}

/// Frozen reply fingerprints use Unicode case folding and Python whitespace.
pub fn reply_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    use unicode_casefold::UnicodeCaseFold;
    let folded: String = text.case_fold().collect();
    let normalized = folded
        .split(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        String::new()
    } else {
        format!("{:x}", Sha256::digest(normalized.as_bytes()))
    }
}

/// Rust regex has no lookbehind; check the frozen phrase exclusions against the
/// prefix of each match instead. Do not broaden ordinary "repeat the test" asks.
pub fn repost_requested(text: &str, owner: &str) -> bool {
    use std::sync::LazyLock;
    static REPOST: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
        r"(?i)\b(re-?post|repeat (?:it|that|this|your)|(?:post|send|say|paste|share) (?:it|that|this|(?:the|your) [\w-]+(?: [\w-]+)?) again|again,? verbatim|one more time)\b"
    ).expect("fixed repost expression")
    });
    if text.contains(&format!("<@{owner}>")) {
        return true;
    }
    let mut start = 0;
    while let Some(m) = REPOST.find_at(text, start) {
        // Only the longest exclusion's suffix matters. Repeated rejected
        // phrases must not repeatedly allocate/fold the whole message prefix.
        let suffix: Vec<_> = text[..m.start()].chars().rev().take(11).collect();
        let prefix = suffix.into_iter().rev().collect::<String>().to_lowercase();
        if ![
            "don't ",
            "don’t ",
            "do not ",
            "never ",
            "ever ",
            "no need to ",
            "should ",
        ]
        .iter()
        .any(|excluded| prefix.ends_with(excluded))
        {
            return true;
        }
        // A rejected outer phrase must not swallow a later overlapping match.
        start = m.start() + text[m.start()..].chars().next().unwrap().len_utf8();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    fn peer(text: &str, status: &str, meta_status: &str, cooling: bool) -> GateInput {
        GateInput {
            owner: "UOWNER".into(),
            text: text.into(),
            sender: "UPEER".into(),
            generated: true,
            meta_kind: "reply".into(),
            meta_status: meta_status.into(),
            peer_turn: 2,
            control: "active".into(),
            status: status.into(),
            turns: 0,
            reset_at: 0.,
            ts: 10.,
            general_messages: true,
            cooling,
            observe_only: false,
            resumed: false,
        }
    }
    #[test]
    fn unaddressed_agent_messages_go_to_triage_unless_finished_blocked_or_cooling() {
        let ask = "I need the repository URL and the full head sha of PR 12";
        let v = attention_gate(&peer(ask, "new", "", false), false);
        assert_eq!((v.kind.as_str(), v.turn), ("triage", 3));
        // The frozen v0.3 gate is unchanged.
        assert_eq!(legacy_gate(&peer(ask, "new", "", false)).kind, "ignore");
        for (status, meta, cooling, kind) in [
            ("new", "complete", false, "ignore"),
            ("new", "blocked", false, "ignore"),
            ("blocked", "", false, "observe"),
            ("new", "", true, "observe"),
        ] {
            assert_eq!(
                attention_gate(&peer(ask, status, meta, cooling), false).kind,
                kind,
                "{status} {meta} {cooling}"
            );
        }
        // An addressed message is answered as before.
        assert_eq!(
            attention_gate(&peer("<@UOWNER> which sha?", "new", "", false), false).kind,
            "respond"
        );
        let mut debrief = peer(ask, "new", "", false);
        debrief.meta_kind = "debrief_root".into();
        assert_eq!(attention_gate(&debrief, false).kind, "ignore");
    }
}
