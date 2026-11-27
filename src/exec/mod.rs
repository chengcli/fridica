//! Trusted subprocess and transport plumbing. No model-generated shell commands
//! or credentialed campaign operations are executed by these helpers.
pub mod artifacts;
pub mod credentials;
pub mod fetch;
pub mod helper;
pub mod isolation;
pub mod local;
pub mod overleaf;
pub mod overleaf_broker;
pub mod probe;
pub mod process;
pub mod python;
pub mod sandbox;
pub mod shell;
pub mod ssh;
pub mod tls;
