//! Configuration edits against the loaded configuration: the host's half of
//! the edit journal in `store::configuration`.
use crate::{
    config::{editor, Config},
    store::Store,
};
use anyhow::{bail, Result};
pub use fridica_core::store::ConfigurationIntent as Intent;
use fridica_core::store::{PendingConfigurationEdit, Store as _};

/// Record a prepared edit's intent and replace the file.
pub async fn replace(store: &Store, edit: editor::Prepared, now: f64) -> Result<()> {
    let intent = Intent {
        path: edit.config.path.clone(),
        before: edit.before.clone(),
        after: edit.config.fingerprint.clone(),
    };
    crate::store::configuration::replace(store, intent, now, move || edit.commit()).await
}
/// Startup adapters were constructed from `config`. An unrenamed edit is
/// explicitly abandoned; a renamed edit is adopted only from exactly that file.
pub async fn recover_startup(store: &Store, config: &Config, now: f64) -> Result<()> {
    if let Some(PendingConfigurationEdit { seq, intent }) =
        store.transact(|u| u.pending_configuration_edit()).await?
    {
        let applied = verify(&intent, config)?;
        editor::sync_directory(&config.path)?;
        store
            .transact(move |u| u.complete_configuration_edit(seq, applied, now))
            .await?;
    }
    Ok(())
}
pub fn verify(intent: &Intent, config: &Config) -> Result<bool> {
    if intent.path != config.path || editor::disk_fingerprint(&config.path)? != config.fingerprint {
        bail!("pending configuration edit conflicts with loaded configuration");
    }
    if config.fingerprint == intent.after {
        Ok(true)
    } else if config.fingerprint == intent.before {
        Ok(false)
    } else {
        bail!("pending configuration edit conflicts with external changes")
    }
}
