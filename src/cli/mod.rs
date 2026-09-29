pub mod assets;
pub mod candidate;
pub mod setup;

/// Preserve the public CLI's distinction between invalid input, an unavailable
/// daemon (including an uncertain/lost reply), and an explicit API rejection.
#[derive(Debug)]
struct ExitFailure {
    code: u8,
    message: String,
}
impl std::fmt::Display for ExitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ExitFailure {}

pub fn input_error(error: anyhow::Error) -> anyhow::Error {
    ExitFailure {
        code: 2,
        message: error.to_string(),
    }
    .into()
}
pub fn control_error(error: anyhow::Error) -> anyhow::Error {
    use crate::control::client::Failure;
    let code = match error.downcast_ref::<Failure>() {
        Some(Failure::Unavailable | Failure::InvalidResponse) => 3,
        Some(Failure::Rejected { .. }) => 4,
        _ => 2,
    };
    ExitFailure {
        code,
        message: error.to_string(),
    }
    .into()
}
pub fn exit_code(error: &anyhow::Error) -> u8 {
    error.downcast_ref::<ExitFailure>().map_or(1, |e| e.code)
}
