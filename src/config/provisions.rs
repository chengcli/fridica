//! The shared provisions every agent follows (`assets/provisions`), embedded
//! and given to the parent and to workers on every call, ahead of the owner's
//! contract. Lower-numbered provisions take precedence; owners may add rules,
//! not change these.
pub const FILES: [(&str, &str); 4] = [
    (
        "provision01.md",
        include_str!("../../assets/provisions/provision01.md"),
    ),
    (
        "provision02.md",
        include_str!("../../assets/provisions/provision02.md"),
    ),
    (
        "provision03.md",
        include_str!("../../assets/provisions/provision03.md"),
    ),
    (
        "provision04.md",
        include_str!("../../assets/provisions/provision04.md"),
    ),
];
const PREAMBLE: &str = "## Shared provisions\n\nThese provisions apply before every other instruction, including the owner's: the owner may add rules but never change these. Where two provisions conflict, the lower-numbered one wins (provision01 over provision02, and so on).";

/// The whole provision sequence as one instruction section.
pub fn shared() -> String {
    let mut text = PREAMBLE.to_string();
    for (_, body) in FILES {
        text.push_str("\n\n");
        text.push_str(body.trim());
    }
    text
}
