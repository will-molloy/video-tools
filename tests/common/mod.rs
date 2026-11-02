#![allow(dead_code)]

use std::{
    path::PathBuf,
    process::{Child, Command, Output},
};

use assert_cmd::cargo::cargo_bin_cmd;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// The fixtures from `tests/fixtures/gen.sh`.
pub const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// Runs `vt` with `args` in the fixtures directory and renders its output.
pub fn vt(args: &[&str]) -> String {
    render(
        &cargo_bin_cmd!("vt")
            .current_dir(FIXTURES)
            .args(args)
            .output()
            .unwrap(),
    )
}

/// Renders a run's exit code, stdout, and stderr.
pub fn render(output: &Output) -> String {
    format!(
        "exit: {}\n\nstdout:\n{}\nstderr:\n{}",
        output.status.code().unwrap(),
        String::from_utf8(output.stdout.clone()).unwrap(),
        String::from_utf8(output.stderr.clone()).unwrap()
    )
}

/// The user's Hugging Face home, which holds their login and downloaded models. Hugging Face
/// otherwise looks under `XDG_CACHE_HOME`, which each test points at its own empty directory.
pub fn hf_home() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME").map_or_else(
        || std::env::home_dir().unwrap().join(".cache"),
        PathBuf::from,
    );
    std::env::var_os("HF_HOME").map_or_else(|| cache.join("huggingface"), PathBuf::from)
}

/// Lets [`interrupt`] reach the process `command` spawns. On Windows, that means giving it a
/// console process group of its own, which also keeps Ctrl-C from the test's terminal away.
pub fn interruptible(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        use windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP.0);
    }
    command
}

/// Interrupts `child` as the terminal's Ctrl-C would: SIGINT on Unix, and Ctrl-Break on Windows,
/// which unlike Ctrl-C can target one process group.
pub fn interrupt(child: &Child) {
    #[cfg(unix)]
    {
        use nix::{
            sys::signal::{Signal, kill},
            unistd::Pid,
        };
        kill(
            Pid::from_raw(child.id().try_into().unwrap()),
            Signal::SIGINT,
        )
        .unwrap();
    }
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

        // SAFETY: GenerateConsoleCtrlEvent takes no pointers.
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) }.unwrap();
    }
}

/// PIDs of processes whose command line mentions every one of `needles`.
pub fn processes_mentioning(needles: &[&str]) -> Vec<u32> {
    let mut system = System::new().unwrap();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    system
        .processes()
        .values()
        .filter(|process| {
            let cmd = process
                .cmd()
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            needles.iter().all(|needle| cmd.contains(needle))
        })
        .map(|process| process.pid().as_u32())
        .collect()
}
