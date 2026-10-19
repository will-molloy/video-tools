mod ffprobe;
mod tools;
mod tui;

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use tools::chapters::ChaptersArgs;

/// Video processing toolbox. Run without a subcommand to open the TUI.
#[derive(Debug, Parser)]
#[command(name = "vt")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print chapters as a YouTube description chapter list
    Chapters(ChaptersArgs),
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    match Cli::parse().command {
        None => tui::run().await.map(|()| ExitCode::SUCCESS),
        Some(Command::Chapters(args)) => tools::chapters::cli(&args).await,
    }
}
