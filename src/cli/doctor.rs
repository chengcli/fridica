//! `fridica doctor`: run the diagnostics and show each result as it arrives.
use super::with_shutdown;
use crate::{
    config::{self, LoadContext},
    doctor::checks::{self, Progress},
};
use anyhow::Result;
use std::{path::PathBuf, time::Duration};

pub async fn run(config: Option<PathBuf>, json: bool, timeout: u64) -> Result<()> {
    let context = LoadContext::current()?;
    let path = config::setup::path(config.as_deref(), &context)?;
    // Each result is shown as soon as it is known: machines are probed
    // at the same time, and a slow host should not look like a hang.
    // JSON stays whole on stdout, so there the lines go to stderr.
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            let line = match event {
                Progress::Configuration(c) => c.text(),
                Progress::Check(c) => c.text(),
            };
            if json {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    });
    let report = with_shutdown(|stop| {
        checks::run_with(
            &path,
            &context,
            std::env::vars_os().collect(),
            Duration::from_secs(timeout),
            stop,
            Some(sender),
        )
    })
    .await;
    // Every sender is gone once the report is detached: the printer
    // then drains what is left and ends.
    let report = report.map(|mut report| {
        report.detach();
        report
    });
    printer.await?;
    let report = report?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", report.summary());
    }
    if !report.passed() {
        anyhow::bail!("doctor checks did not pass");
    }
    Ok(())
}
