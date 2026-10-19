//! Runs an ML stage and streams its protocol messages.
//!
//! `uv run` starts Python as its own child, so the stage runs in a new process group and signals
//! go to the whole group. Otherwise SIGKILL, which uv can't forward, would orphan Python, and a
//! terminal's Ctrl-C would reach Python directly instead of through `vt`'s cancellation.

use std::{collections::VecDeque, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::de::DeserializeOwned;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;

use super::protocol::{self, Done, Message};
use crate::ffprobe::Cancelled;

/// How long a stage gets to clean up after SIGTERM before SIGKILL. Stages check for the signal
/// between chunks, and a chunk can take several seconds on the GPU.
pub const TERM_GRACE: Duration = Duration::from_secs(30);

/// Stderr lines kept to explain a failure.
const STDERR_TAIL: usize = 20;

/// Runs `command`, passing progress and log messages to `on_message`, and returns its result.
///
/// Cancelling `cancel` sends SIGTERM, waits up to `grace` for the group to exit, then sends
/// SIGKILL, and returns [`Cancelled`] once every process in the group is gone.
pub async fn run<R: DeserializeOwned>(
    mut command: Command,
    cancel: &CancellationToken,
    grace: Duration,
    mut on_message: impl FnMut(Message<R>),
) -> Result<Done<R>> {
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to run uv; is it installed and on PATH?")?;
    let group = Group(Pid::from_raw(
        i32::try_from(child.id().context("child exited before it started")?)
            .context("pid out of range")?,
    ));

    let stderr = child.stderr.take().context("stderr not captured")?;
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut tail = VecDeque::with_capacity(STDERR_TAIL);
        while let Ok(Some(line)) = lines.next_line().await {
            if tail.len() == STDERR_TAIL {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        tail
    });

    let mut stdout = BufReader::new(child.stdout.take().context("stdout not captured")?).lines();
    let cancelled = cancel.cancelled();
    tokio::pin!(cancelled);
    let (mut done, mut error) = (None, None);
    loop {
        let line = tokio::select! {
            biased;
            () = &mut cancelled => {
                group.stop(&mut child, grace).await?;
                return Err(Cancelled.into());
            }
            line = stdout.next_line() => line.context("failed to read vt-ml output")?,
        };
        let Some(line) = line else { break };
        let message = match protocol::parse(&line) {
            Ok(message) => message,
            Err(e) => {
                group.stop(&mut child, Duration::ZERO).await?;
                return Err(e);
            }
        };
        match message {
            Message::Done(d) => done = Some(d),
            Message::Error { message } => error = Some(message),
            message => on_message(message),
        }
    }

    let status = child.wait().await.context("failed to wait for vt-ml")?;
    let tail = stderr_task.await.context("stderr reader panicked")?;
    if let Some(message) = error {
        bail!("{message}");
    }
    match done {
        Some(done) if status.success() => Ok(done),
        Some(_) => bail!("vt-ml reported success but exited with {status}"),
        None => {
            let tail: Vec<_> = tail.into();
            bail!(
                "vt-ml exited with {status} before finishing:\n{}",
                tail.join("\n")
            )
        }
    }
}

/// The stage's process group, which shares its leader's pid.
struct Group(Pid);

impl Group {
    async fn stop(&self, child: &mut Child, grace: Duration) -> Result<()> {
        self.signal(Signal::SIGTERM);
        let deadline = Instant::now() + grace;
        if time::timeout_at(deadline, self.exited(child))
            .await
            .is_err()
        {
            self.signal(Signal::SIGKILL);
            self.exited(child).await;
        }
        Ok(())
    }

    /// Waits for the leader, then for any process it left behind.
    async fn exited(&self, child: &mut Child) {
        let _ = child.wait().await;
        while killpg(self.0, None) != Err(Errno::ESRCH) {
            time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn signal(&self, signal: Signal) {
        // Fails only once the group has gone, which is the goal anyway.
        let _ = killpg(self.0, signal);
    }
}

impl Drop for Group {
    /// Backstop for early returns and dropped futures, e.g. when the TUI quits mid-run.
    /// `kill_on_drop` reaches uv only.
    fn drop(&mut self) {
        self.signal(Signal::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde::Deserialize;

    use super::*;
    use crate::ml::protocol::Stats;

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    struct Word {
        word: String,
    }

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    async fn run_sh(script: &str) -> (Result<Done<Word>>, Vec<Message<Word>>) {
        let mut messages = Vec::new();
        let result = run(sh(script), &CancellationToken::new(), TERM_GRACE, |m| {
            messages.push(m);
        })
        .await;
        (result, messages)
    }

    const DONE: &str = r#"{"type": "done", "word": "hi", "device": "cpu", "elapsed_s": 1.5, "peak_vram_mb": null}"#;

    #[tokio::test]
    async fn streams_messages_and_returns_the_result() {
        let script = format!(
            r#"echo '{{"type": "log", "message": "Loading"}}'
               echo '{{"type": "progress", "done": 1, "total": 2}}'
               echo 'stderr noise' >&2
               echo '{{"type": "progress", "done": 2, "total": 2}}'
               echo '{DONE}'"#
        );

        let (result, messages) = run_sh(&script).await;

        assert_eq!(
            result.unwrap(),
            Done {
                result: Word { word: "hi".into() },
                stats: Stats {
                    device: "cpu".into(),
                    elapsed_s: 1.5,
                    peak_vram_mb: None,
                },
            }
        );
        assert_eq!(
            messages,
            [
                Message::Log {
                    message: "Loading".into()
                },
                Message::Progress { done: 1, total: 2 },
                Message::Progress { done: 2, total: 2 },
            ]
        );
    }

    #[tokio::test]
    async fn reports_the_stages_error_message() {
        let script = r#"echo '{"type": "error", "message": "the model is gated"}'
                        exit 2"#;

        let (result, _) = run_sh(script).await;

        assert_eq!(result.unwrap_err().to_string(), "the model is gated");
    }

    #[tokio::test]
    async fn reports_a_child_that_exits_early_with_its_stderr() {
        let script = r#"echo '{"type": "progress", "done": 1, "total": 4}'
                        echo 'Traceback (most recent call last):' >&2
                        echo 'MemoryError' >&2
                        exit 1"#;

        let (result, messages) = run_sh(script).await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "vt-ml exited with exit status: 1 before finishing:\n\
             Traceback (most recent call last):\nMemoryError"
        );
        assert_eq!(messages, [Message::Progress { done: 1, total: 4 }]);
    }

    #[tokio::test]
    async fn reports_a_child_that_dies_from_a_signal() {
        let (result, _) = run_sh("kill -KILL $$").await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "vt-ml exited with signal: 9 (SIGKILL) before finishing:\n"
        );
    }

    #[tokio::test]
    async fn distrusts_success_with_a_failing_exit() {
        let (result, _) = run_sh(&format!("echo '{DONE}'; exit 3")).await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "vt-ml reported success but exited with exit status: 3"
        );
    }

    #[tokio::test]
    async fn stops_the_child_on_a_malformed_line() {
        // Without the stop, the sleep would hold the test for a minute.
        let started = Instant::now();

        let (result, _) = run_sh("echo 'Downloading: 12%'; sleep 60").await;

        assert_eq!(
            result.unwrap_err().to_string(),
            r#"vt-ml wrote a malformed line: "Downloading: 12%""#
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// Runs `script` until its first message, cancels it, and returns how long cancelling took.
    async fn cancel_after_first_message(
        script: &str,
        grace: Duration,
    ) -> (Result<Done<Word>>, Duration) {
        let cancel = CancellationToken::new();
        let mut cancelled_at = None;
        let result = run(sh(script), &cancel, grace, |_| {
            cancelled_at.get_or_insert_with(Instant::now);
            cancel.cancel();
        })
        .await;
        (result, cancelled_at.unwrap().elapsed())
    }

    #[tokio::test]
    async fn cancels_with_sigterm_and_lets_the_child_clean_up() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("output.wav");
        // Like an ML stage, the trap removes partial output before exiting.
        let script = format!(
            r#"trap 'rm {p}; exit 143' TERM
               touch {p}
               echo '{{"type": "progress", "done": 1, "total": 9}}'
               while true; do sleep 0.05; done"#,
            p = partial.display()
        );

        let (result, _) = cancel_after_first_message(&script, TERM_GRACE).await;

        assert!(result.unwrap_err().is::<Cancelled>());
        assert!(!Path::new(&partial).exists());
    }

    #[tokio::test]
    async fn kills_a_child_that_ignores_sigterm() {
        let pid_file = tempfile::NamedTempFile::new().unwrap();
        // The background sleep stands in for Python under uv: a grandchild in the same group.
        let script = format!(
            r#"trap '' TERM
               sleep 60 &
               echo $! > {}
               echo '{{"type": "progress", "done": 1, "total": 9}}'
               wait"#,
            pid_file.path().display()
        );

        let (result, took) = cancel_after_first_message(&script, Duration::from_millis(300)).await;

        assert!(result.unwrap_err().is::<Cancelled>());
        assert!(took < Duration::from_secs(10));
        let grandchild: i32 = std::fs::read_to_string(pid_file.path())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            nix::sys::signal::kill(Pid::from_raw(grandchild), None),
            Err(Errno::ESRCH)
        );
    }
}
