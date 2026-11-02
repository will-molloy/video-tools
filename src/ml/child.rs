//! Runs `vt-ml` and streams its protocol messages.
//!
//! `uv run` starts Python as its own child, so `vt-ml` runs as a process tree: a process group
//! on Unix and a job object on Windows. Stopping the tree reaches Python, which killing uv alone
//! would orphan. The tree is also cut off from the terminal, so Ctrl-C reaches `vt-ml` only
//! through `vt`'s cancellation.

use std::{
    collections::VecDeque,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde::de::DeserializeOwned;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{ChildStdin, Command},
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;

use super::protocol::{self, Done, Message};
use crate::ffprobe::Cancelled;

/// How long `vt-ml` gets to clean up after its stdin closes before it's killed. It checks
/// between chunks, and a chunk can take several seconds on the GPU.
pub const TERM_GRACE: Duration = Duration::from_secs(30);

/// Stderr lines kept to explain a failure.
const STDERR_TAIL: usize = 20;

/// Runs `command`, passing progress and log messages to `on_message`, and returns its result.
///
/// Cancelling `cancel` closes `vt-ml`'s stdin, waits up to `grace` for the tree to exit, then
/// kills it, and returns [`Cancelled`] once every process in the tree is gone.
pub async fn run<R: DeserializeOwned>(
    command: Command,
    cancel: &CancellationToken,
    grace: Duration,
    mut on_message: impl FnMut(Message<R>),
) -> Result<Done<R>> {
    let mut tree = Tree::spawn(command)?;
    let child = &mut tree.child;

    let stderr = child.stderr().take().context("stderr not captured")?;
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut tail = VecDeque::with_capacity(STDERR_TAIL);
        while let Ok(Some(line)) = lines.next_line().await {
            if tail.len() == STDERR_TAIL {
                tail.pop_front();
            }
            tail.push_back(last_redraw(&line).to_string());
        }
        tail
    });

    let mut stdout = BufReader::new(child.stdout().take().context("stdout not captured")?).lines();
    let cancelled = cancel.cancelled();
    tokio::pin!(cancelled);
    let (mut done, mut error) = (None, None);
    loop {
        let line = tokio::select! {
            biased;
            () = &mut cancelled => {
                tree.stop(grace).await;
                return Err(Cancelled.into());
            }
            line = stdout.next_line() => line.context("failed to read vt-ml output")?,
        };
        let Some(line) = line else { break };
        let message = match protocol::parse(&line) {
            Ok(message) => message,
            Err(e) => {
                tree.stop(Duration::ZERO).await;
                return Err(e);
            }
        };
        match message {
            Message::Done(d) => done = Some(d),
            Message::Error { message } => error = Some(message),
            message => on_message(message),
        }
    }

    let status = tree
        .child
        .wait()
        .await
        .context("failed to wait for vt-ml")?;
    let tail = stderr_task.await.context("stderr reader panicked")?;
    if let Some(message) = error {
        bail!("{message}");
    }
    let exit = describe(status);
    match done {
        Some(done) if status.success() => Ok(done),
        Some(_) => bail!("vt-ml reported success but exited with {exit}"),
        None => {
            let tail = Vec::from(tail).join("\n");
            if killed(status) {
                bail!(
                    "vt-ml was killed before finishing ({exit}), probably for running out of \
                     memory:\n{tail}"
                )
            }
            bail!("vt-ml exited with {exit} before finishing:\n{tail}")
        }
    }
}

/// How `status` ended, worded the same on every platform.
fn describe(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),

        // A signal, on Unix.
        None => status.to_string(),
    }
}

/// What a progress bar shows last, since it redraws itself in place with `\r`.
fn last_redraw(line: &str) -> &str {
    line.trim_end_matches('\r')
        .rsplit('\r')
        .next()
        .unwrap_or_default()
}

/// Whether SIGKILL ended `vt-ml`. uv reports its Python child's SIGKILL as exit code 137.
fn killed(status: ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal() == Some(nix::sys::signal::Signal::SIGKILL as i32) {
            return true;
        }
    }
    status.code() == Some(128 + 9)
}

/// `vt-ml`'s process tree.
struct Tree {
    child: Box<dyn ChildWrapper>,

    /// Closing it asks `vt-ml` to stop.
    stdin: Option<ChildStdin>,

    /// The process group, which shares its leader's pid.
    #[cfg(unix)]
    group: nix::unistd::Pid,
}

impl Tree {
    fn spawn(mut command: Command) -> Result<Tree> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut wrap = CommandWrap::from(command);
        #[cfg(unix)]
        wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        {
            use process_wrap::tokio::{CreationFlags, JobObject};
            use windows::Win32::System::Threading::CREATE_NO_WINDOW;

            // A console of its own keeps the terminal's Ctrl-C away.
            wrap.wrap(CreationFlags(CREATE_NO_WINDOW)).wrap(JobObject);
        }
        // On Windows, this also kills the job when it's dropped.
        wrap.wrap(KillOnDrop);

        let mut child = wrap
            .spawn()
            .context("failed to run uv; is it installed and on PATH?")?;
        Ok(Tree {
            stdin: child.stdin().take(),
            #[cfg(unix)]
            group: nix::unistd::Pid::from_raw(
                i32::try_from(child.id().context("child exited before it started")?)
                    .context("pid out of range")?,
            ),
            child,
        })
    }

    /// Closes stdin, waits up to `grace` for the tree to exit, then kills it.
    async fn stop(&mut self, grace: Duration) {
        self.stdin = None;
        let deadline = Instant::now() + grace;
        if time::timeout_at(deadline, self.exited()).await.is_err() {
            // Fails only once the tree has gone, which is the goal anyway.
            let _ = self.child.start_kill();
            self.exited().await;
        }
    }

    /// Waits for every process in the tree to exit.
    async fn exited(&mut self) {
        // On Windows, this waits for the whole job.
        let _ = self.child.wait().await;
        // On Unix, it reaps only `vt`'s own children, so poll for the rest of the group.
        #[cfg(unix)]
        while nix::sys::signal::killpg(self.group, None) != Err(nix::errno::Errno::ESRCH) {
            time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[cfg(unix)]
impl Drop for Tree {
    /// Backstop for early returns and dropped futures, e.g. when the TUI quits mid-run.
    /// `KillOnDrop` reaches uv only.
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(self.group, nix::sys::signal::Signal::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, path::Path};

    use serde::Deserialize;

    use super::*;
    use crate::{ml::protocol::Stats, process};

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    struct Word {
        word: String,
    }

    /// Runs `script` under `uv run`, like `vt-ml`, with `args` in `sys.argv[1:]`.
    fn python(script: &str, args: &[&OsStr]) -> Command {
        let mut command = Command::new("uv");
        command
            .args(["run", "--quiet", "--project"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("ml"))
            .args(["python", "-c", script])
            .args(args);
        command
    }

    async fn run_python(script: &str) -> (Result<Done<Word>>, Vec<Message<Word>>) {
        let mut messages = Vec::new();
        let result = run(
            python(script, &[]),
            &CancellationToken::new(),
            TERM_GRACE,
            |m| messages.push(m),
        )
        .await;
        (result, messages)
    }

    const DONE: &str = r#"{"type": "done", "word": "hi", "device": "cpu", "elapsed_s": 1.5, "peak_vram_mb": null}"#;

    #[tokio::test]
    async fn streams_messages_and_returns_the_result() {
        let script = format!(
            r#"import sys
print('{{"type": "log", "message": "Loading"}}')
print('{{"type": "progress", "done": 1, "total": 2}}')
print("stderr noise", file=sys.stderr)
print('{{"type": "progress", "done": 2, "total": 2}}')
print('{DONE}')"#
        );

        let (result, messages) = run_python(&script).await;

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
    async fn reports_the_error_message() {
        let script = r#"print('{"type": "error", "message": "the model is gated"}')
exit(2)"#;

        let (result, _) = run_python(script).await;

        assert_eq!(result.unwrap_err().to_string(), "the model is gated");
    }

    #[tokio::test]
    async fn reports_a_child_that_exits_early_with_its_stderr() {
        let script = r#"import sys
print('{"type": "progress", "done": 1, "total": 4}')
print("Traceback (most recent call last):", file=sys.stderr)
print("MemoryError", file=sys.stderr)
exit(1)"#;

        let (result, messages) = run_python(script).await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "vt-ml exited with exit code 1 before finishing:\n\
             Traceback (most recent call last):\nMemoryError"
        );
        assert_eq!(messages, [Message::Progress { done: 1, total: 4 }]);
    }

    #[tokio::test]
    async fn keeps_the_last_redraw_of_a_progress_bar() {
        let script = r#"import sys
sys.stderr.write("Loading:   0%\rLoading:  50%\rLoading: 100%\r\n")
print("Killed", file=sys.stderr)
exit(1)"#;

        let (result, _) = run_python(script).await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "vt-ml exited with exit code 1 before finishing:\nLoading: 100%\nKilled"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn suspects_running_out_of_memory_when_killed() {
        // Under uv, the kill would show as uv's exit code instead.
        let mut sh = Command::new("sh");
        sh.args(["-c", "kill -KILL $$"]);

        let result: Result<Done<Word>> =
            run(sh, &CancellationToken::new(), TERM_GRACE, |_| {}).await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "vt-ml was killed before finishing (signal: 9 (SIGKILL)), probably for running out \
             of memory:\n"
        );
    }

    #[tokio::test]
    async fn suspects_running_out_of_memory_when_uv_reports_a_kill() {
        let (result, _) = run_python("exit(137)").await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "vt-ml was killed before finishing (exit code 137), probably for running out of \
             memory:\n"
        );
    }

    #[tokio::test]
    async fn distrusts_success_with_a_failing_exit() {
        let (result, _) = run_python(&format!("print('{DONE}')\nexit(3)")).await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "vt-ml reported success but exited with exit code 3"
        );
    }

    #[tokio::test]
    async fn stops_the_child_on_a_malformed_line() {
        // Without the stop, the sleep would hold the test for a minute.
        let started = Instant::now();

        let (result, _) =
            run_python("import time\nprint('Downloading: 12%', flush=True)\ntime.sleep(60)").await;

        assert_eq!(
            result.unwrap_err().to_string(),
            r#"vt-ml wrote a malformed line: "Downloading: 12%""#
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// Runs `script` with `file` as its argument until its first message, cancels it, and
    /// returns how long cancelling took.
    async fn cancel_after_first_message(
        script: &str,
        file: &Path,
        grace: Duration,
    ) -> (Result<Done<Word>>, Duration) {
        let cancel = CancellationToken::new();
        let mut cancelled_at = None;
        let result = run(python(script, &[file.as_os_str()]), &cancel, grace, |_| {
            cancelled_at.get_or_insert_with(Instant::now);
            cancel.cancel();
        })
        .await;
        (result, cancelled_at.unwrap().elapsed())
    }

    #[tokio::test]
    async fn cancels_by_closing_stdin_and_lets_the_child_clean_up() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("output.wav");
        // Like `vt-ml`, it removes its partial output once stdin closes.
        let script = r#"import pathlib, sys
partial = pathlib.Path(sys.argv[1])
partial.touch()
print('{"type": "progress", "done": 1, "total": 9}', flush=True)
sys.stdin.read()
partial.unlink()
exit(143)"#;

        let (result, _) = cancel_after_first_message(script, &partial, TERM_GRACE).await;

        assert!(result.unwrap_err().is::<Cancelled>());
        assert!(!partial.exists());
    }

    #[tokio::test]
    async fn kills_a_tree_that_ignores_stdin_closing() {
        let pid_file = tempfile::NamedTempFile::new().unwrap();
        // Under uv, the sleeper is a great-grandchild, like a process that Python starts.
        let script = r#"import pathlib, subprocess, sys
sleeper = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
pathlib.Path(sys.argv[1]).write_text(str(sleeper.pid))
print('{"type": "progress", "done": 1, "total": 9}', flush=True)
sys.stdin.read()
sleeper.wait()"#;

        let (result, took) =
            cancel_after_first_message(script, pid_file.path(), Duration::from_millis(300)).await;

        assert!(result.unwrap_err().is::<Cancelled>());
        assert!(took < Duration::from_secs(10));
        let sleeper = std::fs::read_to_string(pid_file.path())
            .unwrap()
            .parse()
            .unwrap();
        assert!(!process::alive(sleeper));
    }
}
