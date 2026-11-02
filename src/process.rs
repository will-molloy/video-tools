//! Process queries that differ between Unix and Windows.

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
