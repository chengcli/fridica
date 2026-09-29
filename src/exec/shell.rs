//! POSIX shell quoting for trusted transport templates, never for model code.
use anyhow::{bail, Result};
pub fn quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c))
    {
        return word.into();
    }
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}
pub fn join(words: &[String]) -> String {
    words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ")
}
pub fn path(word: &str) -> String {
    match word.strip_prefix("~/") {
        Some(tail) => format!("\"$HOME\"/{}", quote(tail)),
        None => quote(word),
    }
}
pub fn env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
pub fn validate(words: &[String]) -> Result<()> {
    if words.is_empty() || words[0].is_empty() || words.iter().any(|s| s.contains('\0')) {
        bail!("invalid command arguments");
    }
    Ok(())
}
