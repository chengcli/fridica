//! Native credential access for trusted adapters. Resolved values are private
//! to the caller and never exported into the daemon's process environment.
use anyhow::{Context, Result};
use fridica_core::secret::{KeyringRef, SecretStore};
use std::ffi::OsStr;

pub use fridica_core::secret::NativeStore;

/// An explicitly supplied environment credential takes precedence, including
/// in existing service units. Only consult the keyring for a configured URI.
pub fn resolve(
    store: &dyn SecretStore,
    variable: &str,
    env: Option<&OsStr>,
    reference: Option<&KeyringRef>,
) -> Result<String> {
    if let Some(value) = env {
        return Ok(value
            .to_str()
            .context("credential must be UTF-8")?
            .to_owned());
    }
    let reference =
        reference.with_context(|| format!("set {variable} or configure a keyring reference"))?;
    let bytes = store
        .get(reference.name())
        .with_context(|| format!("cannot read {} for {variable}", reference.name()))?
        .with_context(|| {
            format!(
                "missing {variable}; run fridica secret set {}",
                reference.name()
            )
        })?;
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("stored credential must be UTF-8"))
}
