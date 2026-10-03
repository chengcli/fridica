//! Configuration edits against the loaded configuration: the host's half of
//! the edit journal in `store::configuration`.
pub use crate::store::configuration::Intent;
use crate::{
    config::{editor, Config},
    store::{
        configuration::{complete, pending},
        Store,
    },
};
use anyhow::{bail, Result};

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
    if let Some((id, intent)) = pending(store).await? {
        let applied = verify(&intent, config)?;
        editor::sync_directory(&config.path)?;
        complete(store, id, applied, now).await?;
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
