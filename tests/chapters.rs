//! Runs `vt chapters` on the fixtures and snapshots its output.

mod common;

use insta::assert_snapshot;

fn chapters(fixture: &str) -> String {
    common::vt(&["chapters", fixture])
}

#[test]
fn valid_chapters() {
    assert_snapshot!(chapters("chapters-valid.mkv"));
}

#[test]
fn chapters_breaking_youtube_rules() {
    assert_snapshot!(chapters("chapters-invalid.mkv"));
}

#[test]
fn chapters_over_an_hour() {
    assert_snapshot!(chapters("chapters-hours.mkv"));
}

#[test]
fn no_chapters() {
    assert_snapshot!(chapters("clean-cfr.mp4"));
}

#[test]
fn heads_each_file_when_given_several() {
    assert_snapshot!(common::vt(&[
        "chapters",
        "chapters-valid.mkv",
        "chapters-hours.mkv"
    ]));
}

#[test]
fn flags_a_file_without_chapters_among_several() {
    assert_snapshot!(common::vt(&[
        "chapters",
        "clean-cfr.mp4",
        "chapters-valid.mkv"
    ]));
}
