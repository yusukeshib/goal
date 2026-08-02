mod config;
mod controller;
mod human;
mod model;
mod prompt;
mod runner;
mod state;

use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "goal", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the foreground goal controller.
    Run {
        #[arg(long, default_value = "goal.toml")]
        config: PathBuf,
    },
}

fn main() -> Result<()> {
    let Cli { command } = Cli::parse();
    match command {
        Command::Run { config } => {
            let loaded = config::LoadedConfig::load(&config)?;
            let cancelled = Arc::new(AtomicBool::new(false));
            let signal_flag = Arc::clone(&cancelled);
            ctrlc::set_handler(move || {
                signal_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })?;
            controller::Controller::new(loaded, cancelled)?.run()?;
        }
    }
    Ok(())
}
