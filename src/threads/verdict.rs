//! Review verdict lines in reports and replies: which pull requests a brief
//! names, where a `VERDICT` line may stand, and what a legacy sign-off
//! becomes. Pure text; the dispatcher signs what these functions find.
use regex::Regex;
use std::sync::LazyLock;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

pub(super) static VERDICT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^VERDICT ([\w.-]+/[\w.-]+)#([0-9]{1,9}) ([0-9a-f]{40}) (approve \(code review\)|approve|changes)$",
    )
    .unwrap()
});
static SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([\w.-]+/[\w.-]+)(?:#|/pull/| (?i:pr|pull) #)([0-9]{1,9})").unwrap()
});
/// The PR a verdict line or a brief names: (lowercased owner/repo, number).
pub(super) fn key(m: &regex::Captures) -> (String, u64) {
    (m[1].to_lowercase(), m[2].parse().unwrap_or(0))
}

/// The PRs a job brief names (`<owner>/<repo>#<PR>` or a pull URL): the only
/// ones its verdict lines may sign.
pub(super) fn subjects(brief: &str) -> Vec<String> {
    let names = SUBJECT.captures_iter(brief);
    names
        .map(|m| format!("{}#{}", key(&m).0, key(&m).1))
        .collect()
}
/// Whether a line holds the word verdict in any case or width (NFKC), with
/// marks (also those NFD splits off a letter) and Unicode's
/// Default_Ignorable_Code_Point characters left out (DerivedCoreProperties-
/// 18.0.0.txt: zero-width characters, the BOM, the soft hyphen, Hangul
/// fillers, variation selectors, tags).
fn says_verdict(line: &str) -> bool {
    let kept = line.nfkc().nfd().filter(|c| {
        !is_combining_mark(*c) && !matches!(c, '\u{AD}' | '\u{34F}' | '\u{61C}' | '\u{115F}'..='\u{1160}' | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
            | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E0FFF}')
            && (c.is_ascii() || c.is_alphanumeric() || c.is_whitespace())
    });
    kept.collect::<String>().to_lowercase().contains("verdict")
}
/// A text's lines holding the word verdict, inside code or not, and the rest.
pub(super) fn verdicts(text: &str) -> (Vec<&str>, Vec<&str>) {
    text.lines().partition(|l| says_verdict(l))
}
const UNPARSED: &str = "Not signed: unparsed verdict line: ";
const PLACED: &str = "Not signed: the verdict line must be the first or last line: ";
/// The text with its verdict lines first, so a cut or split keeps them all in
/// the post; if one has "VERDICT", a line that does not parse as written, or
/// is not the text's first or last non-empty line, becomes its refusal now.
pub(super) fn hoist(text: &str) -> String {
    let (lines, rest) = verdicts(text);
    if !lines.iter().any(|l| l.contains("VERDICT")) {
        return text.into();
    }
    let mut full = text.lines().filter(|l| !l.trim().is_empty());
    let ends = [full.next(), full.next_back()];
    let lines = lines.iter().map(|l| match VERDICT.is_match(l.trim_end()) {
        true if ends.contains(&Some(l)) => l.to_string(),
        true => format!("{PLACED}{}", l.trim()),
        false if l.starts_with(UNPARSED) || l.starts_with(PLACED) => l.to_string(),
        false => format!("{UNPARSED}{}", l.trim()),
    });
    let rest = rest.iter().map(|l| l.to_string());
    lines.chain(rest).collect::<Vec<_>>().join("\n")
}

/// GitHub review mode never turns a worker's legacy protocol line into a Slack
/// approval. Keep the factual prose/links and explicitly label discarded claims.
pub fn github_summary(text: &str) -> String {
    let mut removed = false;
    let mut kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            let upper = line.trim_start().to_ascii_uppercase();
            let legacy = upper.starts_with("SIGN-OFF") || upper.starts_with("VERDICT ");
            removed |= legacy;
            !legacy
        })
        .collect();
    if removed {
        kept.push(
            "Legacy Slack sign-off omitted; that line does not establish GitHub publication.",
        );
    }
    if removed {
        kept.join("\n")
    } else {
        text.to_owned()
    }
}
