use anyhow::Result;
use clap::{Parser, Subcommand};
use fridica::overseer::{plan, Item};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Experimental campaign planner. Does not perform GitHub or Git mutations."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Evaluate explicit validation, CI, and review evidence in a local JSON fixture.
    Plan {
        #[arg(long)]
        input: PathBuf,
    },
}
fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Plan { input } => {
            let item: Item = serde_json::from_slice(&std::fs::read(input)?)?;
            println!("{}", serde_json::to_string_pretty(&plan(&item))?);
        }
    }
    Ok(())
}
