//! Outbox posts as the thread components queue them.
use crate::core::delivery::Post;

/// A post of `kind` in a thread (or, with no `thread_ts`, at a channel's
/// root): no text, metadata, file or ordering until the caller sets them.
pub(super) fn post(
    idem_key: String,
    session: &str,
    kind: &str,
    channel: &str,
    thread_ts: Option<String>,
) -> Post {
    Post {
        idem_key,
        session_id: session.into(),
        kind: kind.into(),
        channel: channel.into(),
        thread_ts,
        text: String::new(),
        meta: None,
        filename: String::new(),
        blob: None,
        after: String::new(),
    }
}
