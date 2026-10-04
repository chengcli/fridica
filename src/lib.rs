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
pub mod slack;
/// Fridica's state, kept by [fridica-store-sqlite](https://github.com/chengcli/fridica-store-sqlite)
/// and reached through the storage contract, [`fridica_core::store`].
pub mod store {
    pub use fridica_store_sqlite::*;
    /// A shared handle to the storage backend. Components hold this and run
    /// units of work through it; only opening, migrating, archiving and
    /// configuration replacement need the SQLite [`Store`] itself.
    pub type Shared = std::sync::Arc<dyn fridica_core::store::Store>;
}
pub mod threads;
pub mod workers;
