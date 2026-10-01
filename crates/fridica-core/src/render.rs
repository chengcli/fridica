//! Pure Slack reply rendering, matched against captured frozen Python cases.
use regex::Regex;
use std::{collections::BTreeSet, sync::LazyLock};
const CONTINUED: &str = "\n\n_The full reply is in the attached details file._";
const ATTACHED: &str = "The reply is in the attached details file.";
pub fn whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}
fn word(c: char) -> bool {
    // Python's \w uses letter/number categories, not Rust's broader Alphabetic
    // property (which also includes some combining marks).
    static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}_]$").unwrap());
    WORD.is_match(c.encode_utf8(&mut [0; 4]))
}

pub fn participants(
    owner: &str,
    messages: impl IntoIterator<Item = (String, String)>,
) -> BTreeSet<String> {
    static MENTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<@([UW][A-Z0-9]+)>").unwrap());
    let mut people = BTreeSet::from([owner.to_owned()]);
    for (sender, text) in messages {
        people.insert(sender);
        for matched in MENTION.captures_iter(&text) {
            people.insert(matched[1].into());
        }
    }
    people
}
pub fn mentions(text: &str, people: &BTreeSet<String>) -> String {
    static PROTECTED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"```[\s\S]*?```|`[^`]*`|<[^>]*>|https?://[^\s<>\x1c-\x1f]+").unwrap()
    });
    static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[UW][A-Z0-9]+").unwrap());
    let replace = |piece: &str| {
        let mut out = String::new();
        let mut last = 0;
        for m in ID.find_iter(piece) {
            let before = piece[..m.start()].chars().next_back();
            let after = piece[m.end()..].chars().next();
            if !before.is_some_and(|c| word(c) || c == '@')
                && !after.is_some_and(word)
                && people.contains(m.as_str())
            {
                out.push_str(&piece[last..m.start()]);
                out.push_str("<@");
                out.push_str(m.as_str());
                out.push('>');
                last = m.end();
            }
        }
        out.push_str(&piece[last..]);
        out
    };
    let mut out = String::new();
    let mut last = 0;
    for m in PROTECTED.find_iter(text) {
        out.push_str(&replace(&text[last..m.start()]));
        out.push_str(m.as_str());
        last = m.end();
    }
    out.push_str(&replace(&text[last..]));
    out
}
pub fn fit(text: &str, details: &str, limit: usize) -> (String, String) {
    let text = text.trim_matches(whitespace);
    if text.is_empty() && !details.is_empty() {
        return (ATTACHED.into(), details.into());
    }
    if text.chars().count() <= limit {
        return (text.into(), details.into());
    }
    let full = if details.is_empty() {
        text.into()
    } else {
        format!("{text}\n\n---\n\n{details}")
    };
    if limit <= CONTINUED.chars().count() + 20 {
        return (text.chars().take(limit.max(1)).collect(), full);
    }
    let budget = limit - CONTINUED.chars().count();
    let window: String = text.chars().take(budget + 1).collect();
    let cut = ["\n\n", "\n", " "]
        .iter()
        .filter_map(|mark| window.rfind(mark).map(|i| window[..i].chars().count()))
        .find(|i| *i > budget / 3)
        .unwrap_or(budget);
    let start: String = text.chars().take(cut).collect();
    (
        format!("{}{CONTINUED}", start.trim_end_matches(whitespace)),
        full,
    )
}
pub fn reply(
    text: &str,
    details: &str,
    waiting: bool,
    requester: &str,
    people: &BTreeSet<String>,
    limit: usize,
) -> (String, String) {
    let mention = format!("<@{requester}>");
    let waiting = waiting && !requester.is_empty();
    let reserve = if waiting {
        mention.chars().count() + 1
    } else {
        0
    };
    let (mut text, details) = fit(
        &mentions(text, people),
        details,
        limit.saturating_sub(reserve),
    );
    if waiting && !text.contains(&mention) {
        text = format!("{mention} {text}");
    }
    (text, details)
}
