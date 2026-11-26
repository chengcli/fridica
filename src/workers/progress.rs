//! Progress-file tracking and privacy-aware notices through the existing outbox.

use crate::core::{delivery::Post, worker::Job};
use anyhow::Result;
use fridica_core::store::Unit;
use serde_json::json;

/// A null/unsent delegating reply is not permission for automatic Slack posts.
/// External drivers have no delegating inbox and retain their own reporting.
pub(crate) fn may_post(u: &mut dyn Unit, job: &Job) -> Result<bool> {
    let Some(inbox) = job.inbox_id else {
        return Ok(false);
    };
    let key = format!("{inbox}:reply");
    Ok(u.thread_posts(&job.session_id)?.iter().any(|p| {
        p.idem_key == key && matches!(p.kind.as_str(), "reply" | "report") && p.state != "failed"
    }))
}

/// One safe blocker per attempt; details remain in authenticated owner controls.
pub(crate) fn approval_notice(u: &mut dyn Unit, job: &Job, owner: &str, now: f64) -> Result<()> {
    if !may_post(u, job)? {
        return Ok(());
    }
    let Some(thread) = u.thread(&job.session_id)? else {
        return Ok(());
    };
    if !u.thread_active(&job.session_id)? {
        return Ok(());
    }
    u.queue_post(&Post {
        idem_key: format!("approval:{}:{}", job.id, job.attempt),
        session_id: job.session_id.clone(),
        kind: "approval_notice".into(),
        channel: thread.channel,
        thread_ts: Some(thread.root_ts),
        text: "This job requested owner approval for an operation outside the saved rules. Check Fridica Approvals for pending requests.".into(),
        meta: Some(json!({"owner":owner,"session":job.session_id,"kind":"approval_notice",
            "job":job.id,"worker":job.worker_id,"v":2})),
        filename: String::new(),
        blob: None,
        after: String::new(),
    }, now)?;
    Ok(())
}

/// Where the next read starts. The file's length when the job started is the
/// first offset, so notes a persistent worker wrote for an earlier job in the
/// same slot are never posted again.
#[derive(Debug, Default)]
pub struct Tracker {
    offset: usize,
}
impl Tracker {
    pub fn starting_at(contents: Option<&[u8]>) -> Self {
        Self {
            offset: contents.map_or(0, <[u8]>::len),
        }
    }
    /// The new complete lines of `contents` as one note: paragraphs (blank
    /// lines apart) trimmed, empty ones dropped, joined by a blank line, and
    /// cut to `cap` characters with a marker. A trailing line without its
    /// newline waits for the next read. A file that shrank was rewritten:
    /// reading restarts at its new end, posting nothing.
    pub fn take(&mut self, contents: &[u8], cap: usize) -> Option<String> {
        if contents.len() < self.offset {
            self.offset = contents.len();
            return None;
        }
        let fresh = &contents[self.offset..];
        let complete = fresh.iter().rposition(|b| *b == b'\n')? + 1;
        self.offset += complete;
        let text = String::from_utf8_lossy(&fresh[..complete]);
        let mut paragraphs = vec![];
        let mut current: Vec<&str> = vec![];
        for line in text.lines() {
            if line.trim().is_empty() {
                if !current.is_empty() {
                    paragraphs.push(current.join("\n"));
                    current.clear();
                }
            } else {
                current.push(line.trim_end());
            }
        }
        if !current.is_empty() {
            paragraphs.push(current.join("\n"));
        }
        let note = paragraphs
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if note.is_empty() {
            return None;
        }
        if note.chars().count() <= cap {
            return Some(note);
        }
        let marker = "\n[… progress note cut]";
        let kept: String = note
            .chars()
            .take(cap.saturating_sub(marker.chars().count()))
            .collect();
        Some(format!("{}{marker}", kept.trim_end()))
    }
}

#[cfg(test)]
mod tests {
    use super::Tracker;
    #[test]
    fn starts_after_earlier_notes_and_holds_back_a_partial_line() {
        let earlier = b"Old job note.\n";
        let mut t = Tracker::starting_at(Some(earlier));
        assert_eq!(t.take(earlier, 100), None);
        let mut file = earlier.to_vec();
        file.extend_from_slice(b"Built the CUDA target.\nRunning ct");
        assert_eq!(
            t.take(&file, 100).as_deref(),
            Some("Built the CUDA target.")
        );
        assert_eq!(t.take(&file, 100), None);
        file.extend_from_slice(b"est.\n");
        assert_eq!(t.take(&file, 100).as_deref(), Some("Running ctest."));
    }
    #[test]
    fn coalesces_paragraphs_and_cuts_long_notes() {
        let mut t = Tracker::starting_at(None);
        let file = b"\n  First stage done.\nNumbers: 3/3.\n\n\n Second stage done. \n\n";
        assert_eq!(
            t.take(file, 100).as_deref(),
            Some("First stage done.\nNumbers: 3/3.\n\nSecond stage done.")
        );
        let mut t = Tracker::starting_at(None);
        let long = format!("{}\n", "x".repeat(500));
        let note = t.take(long.as_bytes(), 200).unwrap();
        assert_eq!(note.chars().count(), 200);
        assert!(note.ends_with("[… progress note cut]"));
    }
    #[test]
    fn a_rewritten_file_restarts_at_its_end_without_posting() {
        let mut t = Tracker::starting_at(None);
        assert!(t.take(b"One.\nTwo.\n", 100).is_some());
        assert_eq!(t.take(b"New\n", 100), None);
        assert_eq!(t.take(b"New\nThree.\n", 100).as_deref(), Some("Three."));
        assert_eq!(t.take(b"New\nThree.\n\n   \n", 100), None);
    }
}
