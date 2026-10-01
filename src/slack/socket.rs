//! Socket Mode from fridica-slack, feeding Fridica's durable receiver.
use super::{receiver::Receiver, web::WebClient};
use crate::core::time::Identifiers;
pub use fridica_slack::socket::{stopped, Acknowledgement, Failure, Options, SocketMode, Status};
use std::sync::Arc;

struct Ids(Arc<dyn Identifiers>);
impl fridica_slack::Ids for Ids {
    fn next(&self, namespace: &str) -> String {
        self.0.next(namespace)
    }
}
/// Socket Mode for `web`, committing envelopes through `receiver`; both must
/// serve the same owner, workspace and channels.
pub fn socket_mode(
    web: Arc<WebClient>,
    receiver: Receiver,
    app_token: String,
    options: Options,
) -> Result<SocketMode, Failure> {
    if !super::web::matches_scope(&web, &receiver.config) {
        return Err(Failure::Configuration);
    }
    let ids = Arc::new(Ids(receiver.ids.clone()));
    SocketMode::new(web, Arc::new(receiver), ids, app_token, options)
}
#[cfg(test)]
#[path = "../../tests/support/slack_socket.rs"]
mod tests;
