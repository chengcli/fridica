//! `fridica dashboard`: serve the control-room page for the running daemon.
use super::{input_error, with_shutdown};
use crate::config::{self, LoadContext};
use anyhow::Result;
use std::path::PathBuf;

pub async fn run(config: Option<PathBuf>, socket: Option<PathBuf>, port: u16) -> Result<()> {
    let socket = match socket {
        Some(socket) => socket,
        None => {
            let context = LoadContext::current()?;
            let path = config.unwrap_or_else(|| context.default_config());
            config::load(&path, &context)
                .map_err(input_error)?
                .state
                .control_socket
        }
    };
    let key = crate::dashboard::key();
    with_shutdown(|mut stopping| {
        let key = key.clone();
        async move {
            crate::dashboard::serve(
                socket,
                port,
                &key,
                |address| {
                    println!("Dashboard: http://{address}/#key={key}");
                    println!("Open it in a browser on this machine. Ctrl-C stops it.");
                },
                async move {
                    let _ = stopping.wait_for(|stop| *stop).await;
                },
            )
            .await
        }
    })
    .await
}
