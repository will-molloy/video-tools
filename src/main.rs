mod ffmpeg;
mod ffprobe;
mod ml;
mod srt;
mod tools;
mod tui;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use tools::{chapters::ChaptersArgs, sync_check::SyncCheckArgs, transcribe::TranscribeArgs};

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

    /// Diagnose audio/video sync problems and recommend a fix
    SyncCheck(SyncCheckArgs),

    /// Write the speech as an editable SRT transcript next to the file
    Transcribe(TranscribeArgs),
}

/// Exit code for errors, distinct from the 1 that tools use for "found a problem".
const ERROR_EXIT: u8 = 2;

#[tokio::main]
async fn main() -> ExitCode {
    let result = match Cli::parse().command {
        None => tui::run().await.map(|()| ExitCode::SUCCESS),
        Some(Command::Chapters(args)) => tools::chapters::cli(&args).await,
        Some(Command::SyncCheck(args)) => tools::sync_check::cli(&args).await,
        Some(Command::Transcribe(args)) => tools::transcribe::cli(&args).await,
    };
    result.unwrap_or_else(|e| {
        eprintln!("Error: {e:#}");
        ExitCode::from(ERROR_EXIT)
    })
}
