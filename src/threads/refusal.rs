//! Why an owner operation was refused, as a value: the control API maps it to
//! a status without reading message text.
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The caller's authority does not allow the operation.
    Forbidden(&'static str),
    /// A retry was asked for and the last decision did not fail.
    NothingToRetry,
    /// The thread's workers are still being stopped.
    CleanupPending,
    /// The operation does not fit the thread's or the daemon's current state.
    Conflict(&'static str),
}
impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Forbidden(message) | Self::Conflict(message) => message,
            Self::NothingToRetry => "nothing to retry",
            Self::CleanupPending => "thread worker cleanup is pending",
        })
    }
}
impl std::error::Error for Refused {}
