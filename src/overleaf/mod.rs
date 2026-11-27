//! Overleaf MCP runtime, worker admission, and broker transport.
pub mod broker;
pub mod git;
pub mod runtime;
pub(crate) mod worker;

use crate::config::overleaf::MAX_SESSION_BYTES;
use anyhow::{ensure, Context, Result};
use std::io::BufRead;

/// Read exactly one newline-terminated grant before the MCP stream.
fn read_grant(input: impl BufRead) -> Result<String> {
    let mut raw = String::new();
    input
        .take((MAX_SESSION_BYTES + 1) as u64)
        .read_line(&mut raw)
        .context("cannot read scoped native session")?;
    ensure!(
        raw.ends_with('\n') && raw.len() > 1,
        "invalid scoped native session transport"
    );
    raw.pop();
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grant_frame_accepts_full_payload_and_preserves_the_next_message() {
        let payload = "x".repeat(MAX_SESSION_BYTES);
        let frame = format!("{payload}\nnext");
        let mut input = frame.as_bytes();
        assert_eq!(read_grant(&mut input).unwrap(), payload);
        assert_eq!(input, b"next");
        assert!(read_grant(format!("{payload}x\n").as_bytes()).is_err());
        assert!(read_grant(payload.as_bytes()).is_err());
    }
}
