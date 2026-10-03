//! `fridica migrate`: the store's database migration, with fridica's
//! configuration file as its companion (migrated, backed up and rolled back
//! with the database).
use crate::{
    config,
    store::migration::{self, Companion, Plan},
};
use anyhow::{bail, Result};
use std::path::Path;

/// `config.toml`: its text migration, its `state.path` check, and its
/// fingerprinted replacement.
pub struct ConfigFile;
impl Companion for ConfigFile {
    fn migrate_text(&self, source: &str) -> Result<String> {
        config::migrate_text(source)
    }
    fn check(&self, source: &str, path: &Path, database: &Path) -> Result<()> {
        let context = config::LoadContext::current()?;
        let resolved = config::loader::parse(source, path, &context)?;
        let target =
            config::loader::resolve_path(database, &std::env::current_dir()?, &context.home)?;
        if resolved.state.path != target {
            bail!("migration database differs from configuration state.path");
        }
        Ok(())
    }
    fn replace(&self, path: &Path, expected: &str, text: &str) -> Result<()> {
        config::editor::replace(path, expected, text)
    }
}

pub fn dry_run(database: &Path, configuration: &Path) -> Result<Plan> {
    migration::dry_run(database, configuration, &ConfigFile)
}
pub fn migrate(database: &Path, configuration: &Path, now: f64) -> Result<Plan> {
    migration::migrate(database, configuration, now, &ConfigFile)
}
pub fn rollback(database: &Path, configuration: &Path) -> Result<()> {
    migration::rollback(database, configuration, &ConfigFile)
}
