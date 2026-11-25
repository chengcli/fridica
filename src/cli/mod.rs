pub mod assets;
pub mod candidate;
pub mod dashboard;
pub mod doctor;
pub mod migrate;
pub mod overleaf_auth;
pub mod secret;
pub mod setup;
pub mod start;

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

/// What the owner is told: the error with its causes, on one line. Only the
/// first line, so a parser's source excerpt never appears here.
fn render(error: &anyhow::Error) -> String {
    format!("{error:#}").lines().next().unwrap_or("").to_owned()
}
pub fn input_error(error: anyhow::Error) -> anyhow::Error {
    ExitFailure {
        code: 2,
        message: render(&error),
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
        message: render(&error),
    }
    .into()
}
pub fn exit_code(error: &anyhow::Error) -> u8 {
    error.downcast_ref::<ExitFailure>().map_or(1, |e| e.code)
}

/// Install handlers before starting probes or durable/network service effects.
/// The operation owns cleanup after stop; never abandon its future on a signal.
pub async fn with_shutdown<T, F, Fut>(run: F) -> anyhow::Result<T>
where
    F: FnOnce(tokio::sync::watch::Receiver<bool>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let running = run(stopping);
    tokio::pin!(running);
    tokio::select! {
        result=&mut running => result,
        _=async { tokio::select! { _=interrupt.recv()=>{}, _=terminate.recv()=>{} } } => {
            stop.send_replace(true);
            running.await
        },
    }
}
