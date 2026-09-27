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
