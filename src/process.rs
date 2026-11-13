//! Child processes, and process queries that differ between Unix and Windows.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;

use crate::ffprobe::Cancelled;

/// Runs `command`, passing each line of its stdout to `on_line` as it arrives, so the output is
/// never buffered whole. Returns its stderr once it exits successfully. `name` names the program
/// in errors.
///
/// Cancelling `cancel` kills the program, waits for it to exit, and returns [`Cancelled`].
pub async fn stream_lines(
    mut command: Command,
    name: &str,
    cancel: &CancellationToken,
    mut on_line: impl FnMut(&str) -> Result<()>,
) -> Result<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Backstop for the early returns below. Cancellation kills explicitly so it can also reap
        // the process.
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to run {name}; is it installed and on PATH?"))?;

    // Drain stderr concurrently. A pipe nobody reads fills up and blocks the program mid-write.
    let mut stderr = child
        .stderr
        .take()
        .with_context(|| format!("{name} stderr not captured"))?;
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await.map(|_| buf)
    });

    let mut stdout = BufReader::new(
        child
            .stdout
            .take()
            .with_context(|| format!("{name} stdout not captured"))?,
    );
    let mut line = String::new();

    // Created once and polled by reference, so each loop iteration doesn't register a new waiter.
    let cancelled = cancel.cancelled();
    tokio::pin!(cancelled);

    loop {
        line.clear();
        // `biased` polls cancellation first rather than in random order, so a steady stream of
        // ready lines can't delay it. `read_line` loses partial input when the other branch wins,
        // which doesn't matter because cancelling abandons the output.
        let read = tokio::select! {
            biased;
            () = &mut cancelled => {
                child.kill().await.with_context(|| format!("failed to stop {name}"))?;
                return Err(Cancelled.into());
            }
            read = stdout.read_line(&mut line) => {
                read.with_context(|| format!("failed to read {name} output"))?
            }
        };
        if read == 0 {
            break;
        }
        on_line(line.trim_end())?;
    }

    let status = child
        .wait()
        .await
        .with_context(|| format!("failed to wait for {name}"))?;
    let stderr = stderr_task
        .await
        .with_context(|| format!("{name} stderr reader panicked"))?
        .with_context(|| format!("failed to read {name} stderr"))?;
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    if !status.success() {
        bail!("{name} failed: {}", stderr.trim());
    }
    Ok(stderr)
}

/// Whether a process with `pid` exists and hasn't exited.
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

    // Pids above i32::MAX don't exist, and 0 or less would signal a group instead.
    match i32::try_from(pid) {
        Ok(pid) if pid > 0 => kill(Pid::from_raw(pid), None) != Err(Errno::ESRCH),
        _ => false,
    }
}

/// Whether a process with `pid` exists and hasn't exited.
#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    // SAFETY: OpenProcess takes no pointers, and the handle is closed below.
    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(process) => process,

        // Only a process that exists can refuse access.
        Err(e) => return e.code() == ERROR_ACCESS_DENIED.to_hresult(),
    };
    let mut code = 0;
    // SAFETY: `process` is open, and `code` outlives the call.
    let running = unsafe { GetExitCodeProcess(process, &raw mut code) }.is_ok()
        && code == STILL_ACTIVE.0.cast_unsigned();
    // SAFETY: `process` is open and not used again.
    let _ = unsafe { CloseHandle(process) };
    running
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_this_process() {
        assert!(alive(std::process::id()));
    }

    #[test]
    fn misses_an_exited_process() {
        let mut child = std::process::Command::new("cargo")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();

        assert!(!alive(pid));
    }
}
