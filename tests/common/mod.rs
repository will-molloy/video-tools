use std::process::Output;

use assert_cmd::cargo::cargo_bin_cmd;

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
