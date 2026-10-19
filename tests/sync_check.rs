//! Runs `vt sync-check` on the fixtures and snapshots its output.

mod common;

use insta::assert_snapshot;

fn sync_check(fixture: &str) -> String {
    common::vt(&["sync-check", fixture])
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
