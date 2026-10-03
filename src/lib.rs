pub mod approvals;
pub mod attention;
pub mod cli;
pub mod config;
pub mod control;
pub use fridica_core as core;
pub mod daemon;
pub mod dashboard;
pub mod doctor;
pub mod exec;
pub mod github;
pub mod machines;
pub mod parent;
pub mod report;
pub mod slack;
/// Fridica's state, kept by [fridica-store-sqlite](https://github.com/chengcli/fridica-store-sqlite).
pub use fridica_store_sqlite as store;
pub mod threads;
pub mod workers;
