//! Runs `vt sync-check` on the fixtures and snapshots its output. Fixing runs on a copy of each
//! fixture in a scratch directory.

mod common;

use std::{fs, path::Path};

use assert_cmd::cargo::cargo_bin_cmd;
use insta::assert_snapshot;
use tempfile::TempDir;

fn sync_check(fixture: &str) -> String {
    common::vt(&["sync-check", fixture])
}

/// A scratch directory holding a copy of `fixture`.
fn copy_of(fixture: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::copy(
        Path::new(common::FIXTURES).join(fixture),
        dir.path().join(fixture),
    )
    .unwrap();
    dir
}

/// Runs `vt` with `args` in `dir` and renders its output.
fn vt_in(dir: &TempDir, args: &[&str]) -> String {
    common::render(
        &cargo_bin_cmd!("vt")
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap(),
    )
}

/// Runs `vt sync-check --fix` on a copy of `fixture`, and renders its output and the files it
/// left behind.
fn fix(fixture: &str) -> String {
    let dir = copy_of(fixture);
    let output = vt_in(&dir, &["sync-check", "--fix", fixture]);
    format!("{output}\nfiles:\n{}", files(&dir))
}

fn files(dir: &TempDir) -> String {
    let mut names: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.join("\n")
}

#[test]
fn audio_early() {
    assert_snapshot!(sync_check("audio-early.mp4"));
}

#[test]
fn fixes_jitter_by_retiming() {
    assert_snapshot!(fix("vfr-jitter.mp4"));
}

#[test]
fn fixes_dropped_frames_by_reencoding() {
    assert_snapshot!(fix("dropped-frames.mp4"));
}

#[test]
fn fixes_late_audio_by_padding() {
    assert_snapshot!(fix("audio-offset.mp4"));
}

#[test]
fn fixes_early_audio_by_trimming() {
    assert_snapshot!(fix("audio-early.mp4"));
}

#[test]
fn fixes_an_audio_gap_by_filling() {
    assert_snapshot!(fix("audio-gap.mp4"));
}

#[test]
fn writes_nothing_when_nothing_has_an_automatic_fix() {
    assert_snapshot!(fix("audio-short-tail.mp4"));
}

#[test]
fn writes_nothing_for_a_clean_file() {
    assert_snapshot!(fix("clean-cfr.mp4"));
}

#[test]
fn keeps_an_existing_fixed_copy() {
    let dir = copy_of("audio-gap.mp4");
    fs::write(dir.path().join("audio-gap.synced.mp4"), "mine").unwrap();

    assert_snapshot!(vt_in(&dir, &["sync-check", "--fix", "audio-gap.mp4"]));
    assert_eq!(
        fs::read_to_string(dir.path().join("audio-gap.synced.mp4")).unwrap(),
        "mine"
    );
}

#[test]
fn replaces_an_existing_fixed_copy_when_forced() {
    let dir = copy_of("audio-gap.mp4");
    fs::write(dir.path().join("audio-gap.synced.mp4"), "mine").unwrap();

    assert_snapshot!(vt_in(
        &dir,
        &["sync-check", "--fix", "--force", "audio-gap.mp4"]
    ));
}

#[test]
fn clean_cfr_mp4() {
    assert_snapshot!(sync_check("clean-cfr.mp4"));
}

#[test]
fn clean_cfr_mkv() {
    assert_snapshot!(sync_check("clean-cfr.mkv"));
}

#[test]
fn vfr_jitter() {
    assert_snapshot!(sync_check("vfr-jitter.mp4"));
}

#[test]
fn dropped_frames() {
    assert_snapshot!(sync_check("dropped-frames.mp4"));
}

#[test]
fn audio_offset() {
    assert_snapshot!(sync_check("audio-offset.mp4"));
}

#[test]
fn audio_gap() {
    assert_snapshot!(sync_check("audio-gap.mp4"));
}

#[test]
fn audio_short_tail() {
    assert_snapshot!(sync_check("audio-short-tail.mp4"));
}

#[test]
fn missing_file_exits_2() {
    assert_snapshot!(sync_check("missing.mp4"));
}

#[test]
fn checks_the_other_files_when_one_is_missing() {
    assert_snapshot!(common::vt(&[
        "sync-check",
        "clean-cfr.mp4",
        "missing.mp4",
        "clean-cfr.mkv"
    ]));
}
