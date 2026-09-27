//! Trusted subprocess and transport plumbing. No model-generated shell commands
//! or credentialed campaign operations are executed by these helpers.
pub mod local;
pub mod process;
pub mod sandbox;
pub mod shell;
pub mod ssh;
