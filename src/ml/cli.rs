//! Runs an ML tool from the command line.

use std::{
    fmt::Display,
    io::{IsTerminal, Write},
    process::ExitCode,
};

use anyhow::{Context, Result};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::ffprobe::Cancelled;

const SIGINT: u8 = 2;
const SIGTERM: u8 = 15;

/// Awaits `run`, then prints its outcome's `lines`. Exits 0 on success, or 128 plus the signal
/// number when SIGINT or SIGTERM cancels it. Errors exit 2 from `main`.
///
/// A signal cancels `cancel`, and the run keeps going until it has cleaned up, so a stage's
/// subprocess never outlives `vt`.
pub async fn run<T>(
    cancel: &CancellationToken,
    run: impl Future<Output = Result<T>>,
    lines: impl FnOnce(T) -> Vec<String>,
) -> Result<ExitCode> {
    let mut interrupt = signal(SignalKind::interrupt()).context("failed to handle SIGINT")?;
    let mut terminate = signal(SignalKind::terminate()).context("failed to handle SIGTERM")?;
    tokio::pin!(run);

    let mut signalled = None;
    let result = loop {
        tokio::select! {
            result = &mut run => break result,
            _ = interrupt.recv(), if signalled.is_none() => signalled = Some(SIGINT),
            _ = terminate.recv(), if signalled.is_none() => signalled = Some(SIGTERM),
        }
        cancel.cancel();
    };
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K");
    }

    if let Some(signal) = signalled
        && result.as_ref().is_err_and(|e| e.is::<Cancelled>())
    {
        eprintln!("Cancelled.");
        return Ok(ExitCode::from(128 + signal));
    }
    for line in lines(result?) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Shows `progress` on one line of stderr, when it's a terminal.
pub fn show_progress(progress: &impl Display) {
    let mut stderr = std::io::stderr();
    if stderr.is_terminal() {
        let _ = write!(stderr, "\r\x1b[2K{progress}");
        let _ = stderr.flush();
    }
}
