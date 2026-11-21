//! `fridica start`: the daemon, active or observe-only, or the read-only
//! startup readiness report.
use super::{input_error, with_shutdown};
use crate::config::{self, LoadContext};
use anyhow::Result;
use std::{path::PathBuf, time::Duration};

pub async fn run(
    config: Option<PathBuf>,
    observe_only: bool,
    check_ready: bool,
    timeout: Option<u64>,
) -> Result<()> {
    let context = LoadContext::current()?;
    let path = config.unwrap_or_else(|| context.default_config());
    let config = config::load(&path, &context).map_err(input_error)?;
    if check_ready {
        let report = with_shutdown(|stop| async move {
            crate::doctor::readiness::check(
                &config,
                &context,
                std::env::vars_os().collect(),
                Duration::from_secs(timeout.unwrap_or(30)),
                stop,
            )
            .await
        })
        .await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        if !report.startup_checks_passed {
            anyhow::bail!("startup readiness checks did not pass");
        }
        return Ok(());
    }
    let credentials = crate::daemon::Credentials::read(&config, |name| std::env::var(name).ok())
        .map_err(input_error)?;
    if observe_only {
        with_shutdown(|stop| crate::daemon::observe(config, credentials, stop)).await
    } else {
        // Active by default: readiness and doctor rerun before Slack I/O.
        with_shutdown(|stop| {
            crate::daemon::active(
                config,
                context,
                std::env::vars_os().collect(),
                credentials,
                stop,
            )
        })
        .await
    }
}
