//! Runs `vt sync-check` on the fixtures from `tests/fixtures/gen.sh` and snapshots its output.

use assert_cmd::cargo::cargo_bin_cmd;
use insta::assert_snapshot;

/// Runs the command in the fixtures directory and renders its exit code and stdout.
fn sync_check(args: &[&str]) -> String {
    let output = cargo_bin_cmd!("vt")
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"))
        .arg("sync-check")
        .args(args)
        .output()
        .unwrap();
    let code = output.status.code().unwrap();
    format!(
        "exit: {code}\n\n{}{}",
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap()
    )
}

/// Snapshots the human and JSON reports for one fixture.
fn check(fixture: &str) {
    assert_snapshot!(format!("{fixture}.human"), sync_check(&[fixture]));
    assert_snapshot!(format!("{fixture}.json"), sync_check(&["--json", fixture]));
}

#[test]
fn clean_cfr_mp4() {
    check("clean-cfr.mp4");
}

#[test]
fn clean_cfr_mkv() {
    check("clean-cfr.mkv");
}

#[test]
fn vfr_jitter() {
    check("vfr-jitter.mp4");
}

#[test]
fn dropped_frames() {
    check("dropped-frames.mp4");
}

#[test]
fn audio_offset() {
    check("audio-offset.mp4");
}

#[test]
fn audio_gap() {
    check("audio-gap.mp4");
}

#[test]
fn audio_short_tail() {
    check("audio-short-tail.mp4");
}

#[test]
fn missing_file_exits_2() {
    assert_snapshot!(sync_check(&["missing.mp4"]));
}
