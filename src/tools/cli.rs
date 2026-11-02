//! Runs a tool on each file from the command line.

use std::{
    collections::BTreeMap,
    fmt::Display,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result};
use futures::{Stream, StreamExt, stream};
use tokio_util::sync::CancellationToken;

use super::Tool;
use crate::{display, ffprobe::Cancelled};

/// Exit code for errors, distinct from the 1 that tools use for "found a problem".
pub const ERROR_EXIT: u8 = 2;

/// What a tool prints for one file.
#[derive(Debug, Default)]
pub struct Printed {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,

    /// Whether the tool found a problem, such as a sync issue.
    pub flagged: bool,
}

/// Runs `each` on `files`, up to the tool's parallel limit at once, and prints the results in
/// input order. A file's error doesn't stop the others.
///
/// Exits with [`Stop::exit_code`] when a [`Stop`] cancels the run, 2 if any file errored, 1 if
/// any file was flagged, and 0 otherwise.
///
/// A stop cancels `cancel`, and the run keeps going until it has cleaned up, so a subprocess
/// never outlives `vt`. Files that haven't started are skipped.
pub async fn run<'a, Fut>(
    tool: Tool,
    files: &'a [PathBuf],
    cancel: &CancellationToken,
    each: impl Fn(&'a Path) -> Fut,
) -> Result<ExitCode>
where
    Fut: Future<Output = Result<Printed>>,
{
    let mut stops = Stops::new()?;

    let each = &each;
    let results = as_they_finish(files, tool.max_parallel(), |file| async move {
        if cancel.is_cancelled() {
            Err(Cancelled.into())
        } else {
            each(file).await
        }
    });
    tokio::pin!(results);

    let mut finished = BTreeMap::new();
    let mut next = 0;
    let mut output = Output::new(files.len() > 1);
    let mut stopped = None;
    loop {
        tokio::select! {
            result = results.next() => {
                let Some((index, result)) = result else { break };
                finished.insert(index, result);
                clear_progress();
                while let Some(result) = finished.remove(&next) {
                    output.print(&files[next], result);
                    next += 1;
                }
            }
            stop = stops.recv(), if stopped.is_none() => {
                stopped = Some(stop);
                cancel.cancel();
            }
        }
    }

    if let Some(stop) = stopped
        && output.cancelled
    {
        eprintln!("Cancelled.");
        return Ok(stop.exit_code());
    }
    Ok(output.exit_code())
}

/// A request from the terminal or the OS to stop the run.
#[derive(Debug, Clone, Copy)]
enum Stop {
    /// SIGINT, or Ctrl-C or Ctrl-Break on Windows.
    Interrupt,

    /// SIGTERM, or the console closing on Windows.
    Terminate,
}

impl Stop {
    /// 128 plus the Unix signal number, as shells report it, on every platform.
    fn exit_code(self) -> ExitCode {
        ExitCode::from(match self {
            Stop::Interrupt => 128 + 2,
            Stop::Terminate => 128 + 15,
        })
    }
}

/// Listens for each [`Stop`].
#[cfg(unix)]
struct Stops {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Stops {
    fn new() -> Result<Stops> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Stops {
            interrupt: signal(SignalKind::interrupt()).context("failed to handle SIGINT")?,
            terminate: signal(SignalKind::terminate()).context("failed to handle SIGTERM")?,
        })
    }

    async fn recv(&mut self) -> Stop {
        tokio::select! {
            _ = self.interrupt.recv() => Stop::Interrupt,
            _ = self.terminate.recv() => Stop::Terminate,
        }
    }
}

/// Listens for each [`Stop`].
#[cfg(windows)]
struct Stops {
    ctrl_c: tokio::signal::windows::CtrlC,
    ctrl_break: tokio::signal::windows::CtrlBreak,
    ctrl_close: tokio::signal::windows::CtrlClose,
}

#[cfg(windows)]
impl Stops {
    fn new() -> Result<Stops> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};
        Ok(Stops {
            ctrl_c: ctrl_c().context("failed to handle Ctrl-C")?,
            ctrl_break: ctrl_break().context("failed to handle Ctrl-Break")?,
            ctrl_close: ctrl_close().context("failed to handle the console closing")?,
        })
    }

    async fn recv(&mut self) -> Stop {
        tokio::select! {
            _ = self.ctrl_c.recv() => Stop::Interrupt,
            _ = self.ctrl_break.recv() => Stop::Interrupt,
            _ = self.ctrl_close.recv() => Stop::Terminate,
        }
    }
}

/// Runs `each` on `files` and yields each result with its file's index as it finishes. The next
/// file starts as soon as any running file finishes.
fn as_they_finish<'a, T, Fut>(
    files: &'a [PathBuf],
    limit: usize,
    each: impl Fn(&'a Path) -> Fut,
) -> impl Stream<Item = (usize, T)>
where
    Fut: Future<Output = T>,
{
    stream::iter(files.iter().enumerate().map(move |(index, file)| {
        let run = each(file);
        async move { (index, run.await) }
    }))
    .buffer_unordered(limit)
}

/// Prints each file's result and remembers how the run went.
struct Output {
    /// Whether output names its file, which it needs when there are several.
    several: bool,
    printed_any: bool,
    errored: bool,
    flagged: bool,
    cancelled: bool,
}

impl Output {
    fn new(several: bool) -> Self {
        Output {
            several,
            printed_any: false,
            errored: false,
            flagged: false,
            cancelled: false,
        }
    }

    fn print(&mut self, file: &Path, result: Result<Printed>) {
        match result {
            Ok(printed) => {
                self.flagged |= printed.flagged;
                self.print_stdout(file, &printed.stdout);
                for line in &printed.stderr {
                    eprintln!("{}", display::plain(line));
                }
            }
            Err(e) if e.is::<Cancelled>() => self.cancelled = true,
            Err(e) => {
                self.errored = true;
                let name = if self.several {
                    format!("{}: ", display::path(file))
                } else {
                    String::new()
                };
                eprintln!("Error: {}", display::plain(&format!("{name}{e:#}")));
            }
        }
    }

    fn print_stdout(&mut self, file: &Path, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        if self.several {
            if self.printed_any {
                println!();
            }
            println!("==> {} <==", display::plain(&display::path(file)));
        }
        self.printed_any = true;
        for line in lines {
            println!("{}", display::plain(line));
        }
    }

    fn exit_code(&self) -> ExitCode {
        if self.errored {
            ExitCode::from(ERROR_EXIT)
        } else if self.flagged {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    }
}

/// Shows `progress` on `file` on one line of stderr, when it's a terminal.
pub fn show_progress(file: &Path, progress: &impl Display) {
    let mut stderr = std::io::stderr();
    if stderr.is_terminal() {
        let name = display::plain(&display::name(file));
        let _ = write!(stderr, "\r\x1b[2K{name}: {progress}");
        let _ = stderr.flush();
    }
}

fn clear_progress() {
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{sync::Notify, time::timeout};

    use super::*;

    /// `a` runs until `c` starts, so `c` must start in the slot `b` frees while `a` still runs.
    #[tokio::test]
    async fn starts_the_next_file_when_a_later_file_finishes_first() {
        let files: Vec<PathBuf> = ["a", "b", "c"].map(PathBuf::from).into();
        let c_started = Notify::new();

        let results = as_they_finish(&files, 2, |file| {
            let c_started = &c_started;
            async move {
                match file.to_str() {
                    Some("a") => c_started.notified().await,
                    Some("c") => c_started.notify_one(),
                    _ => {}
                }
            }
        });
        let finished = timeout(Duration::from_secs(5), results.collect::<Vec<_>>())
            .await
            .unwrap();

        assert_eq!(
            finished
                .iter()
                .map(|(index, ())| *index)
                .collect::<Vec<_>>(),
            [1, 2, 0]
        );
    }
}
