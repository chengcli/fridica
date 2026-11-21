//! Owner-managed entries in the operating system's credential store.
use anyhow::{ensure, Result};
use clap::Subcommand;
use std::io::{self, Write};

use crate::core::secret::{KeyringRef, SecretStore};
use crate::secrets::NativeStore;

#[derive(Subcommand)]
pub enum Command {
    /// Prompt for a new or replacement secret with terminal echo disabled.
    Set { name: String },
    /// Write the secret value to stdout.
    Get { name: String },
    /// Remove a secret from the OS credential store.
    Delete { name: String },
    /// List stored Fridica references without reading their values.
    List,
}

fn reference(name: &str) -> Result<String> {
    Ok(KeyringRef::named(name)?.name().to_owned())
}

pub fn get(name: &str) -> Result<()> {
    let value = NativeStore
        .get(&reference(name)?)?
        .ok_or_else(|| anyhow::anyhow!("secret not found"))?;
    let mut output = io::stdout().lock();
    output.write_all(&value)?;
    output.flush()?;
    Ok(())
}

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Set { name } => {
            let name = reference(&name)?;
            let value = rpassword::prompt_password(format!("New value for {name}: "))?;
            ensure!(
                !value.is_empty() && !value.contains('\0'),
                "expected a nonempty text secret"
            );
            NativeStore.set(&name, value.as_bytes())
        }
        Command::Get { name } => get(&name),
        Command::Delete { name } => NativeStore.delete(&reference(&name)?),
        Command::List => {
            let mut names = NativeStore.list()?;
            names.sort_unstable();
            names.dedup();
            for name in names {
                if reference(&name).is_ok_and(|canonical| canonical == name) {
                    writeln!(io::stdout().lock(), "{name}")?;
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_accepts_short_names_and_template_paths() {
        assert_eq!(
            reference("slack/app-token").unwrap(),
            "fridica/slack/app-token"
        );
        for line in include_str!("../../../assets/secrets.yml").lines() {
            if let Some((_, path)) = line.split_once("!var ") {
                assert_eq!(reference(path).unwrap(), path);
            }
        }
        for path in [
            "",
            "fridica/",
            "../slack",
            "/slack",
            "slack//app",
            "slack\napp",
        ] {
            assert!(reference(path).is_err());
        }
    }
}
