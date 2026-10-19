use assert_cmd::cargo::cargo_bin_cmd;

/// Runs `vt` with `args` in the fixtures directory from `tests/fixtures/gen.sh` and renders its
/// exit code, stdout, and stderr.
pub fn vt(args: &[&str]) -> String {
    let output = cargo_bin_cmd!("vt")
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"))
        .args(args)
        .output()
        .unwrap();
    format!(
        "exit: {}\n\nstdout:\n{}\nstderr:\n{}",
        output.status.code().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap()
    )
}
