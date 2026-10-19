mod tools;
mod tui;

use anyhow::Result;
use clap::Parser;

/// Video processing toolbox.
#[derive(Debug, Parser)]
#[command(name = "vt")]
struct Cli {}

#[tokio::main]
async fn main() -> Result<()> {
    Cli::parse();
    tui::run().await
}
