//! The fixed prompt text Fridica adds to the owner's contract
//! (`assets/prompts`), embedded at build time. Each file ends with one
//! newline, which is not part of the text.

/// What the parent may do in a decide or repair turn, field by field.
pub const ACTION: &str = include_str!("../../assets/prompts/parent-action.md").trim_ascii_end();
/// The triage call's answer.
pub const TRIAGE: &str = include_str!("../../assets/prompts/parent-triage.md").trim_ascii_end();
/// The debrief call's answer.
pub const DEBRIEF: &str = include_str!("../../assets/prompts/parent-debrief.md").trim_ascii_end();
/// Closes every parent and worker prompt, before the data.
pub const UNTRUSTED: &str = include_str!("../../assets/prompts/untrusted.md").trim_ascii_end();
