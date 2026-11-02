//! Runs `vt transcribe` with the fake backend, which needs no ML dependencies and reads each
//! loud stretch of audio as a line of speech.
//!
//! The `#[ignore]`d test at the end runs Whisper and needs the ML stack and a GPU.

mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use assert_cmd::cargo::cargo_bin;
use common::hf_home;
use insta::{Settings, assert_snapshot};
use tempfile::TempDir;

const FAKE_DELAY_ENV: &str = "VT_ML_FAKE_DELAY_S";

/// The fake backend's transcript of `speech-bursts.mkv`.
const BURSTS_SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\nspeech\n\n\
                          2\n00:00:03,500 --> 00:00:04,500\nspeech\n\n";

/// A scratch directory holding a copy of a fixture and an empty cache.
struct Workspace {
    dir: TempDir,
}

impl Workspace {
    fn with(fixture: &str) -> Workspace {
        let workspace = Workspace {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::copy(
            Path::new(common::FIXTURES).join(fixture),
            workspace.path(fixture),
        )
        .unwrap();
        workspace
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn cache(&self) -> PathBuf {
        self.path("cache").join("video-tools/transcribe")
    }

    fn command(&self, backend: &str, args: &[&str]) -> Command {
        // A std `Command`, unlike assert_cmd's, can spawn without waiting, for the cancel test.
        let mut command = Command::new(cargo_bin!("vt"));
        command
            .current_dir(self.dir.path())
            .env("HF_HOME", hf_home())
            .env("XDG_CACHE_HOME", self.path("cache"))
            .args(["transcribe", "--backend", backend])
            .args(args);
        command
    }

    fn vt(&self, args: &[&str]) -> String {
        common::render(&self.command("fake", args).output().unwrap())
    }

    /// File names in the workspace, excluding the cache.
    fn files(&self) -> Vec<String> {
        names(self.dir.path())
            .into_iter()
            .filter(|name| name != "cache")
            .collect()
    }

    /// Scratch directories left in the cache.
    fn scratch(&self) -> Vec<String> {
        names(&self.cache())
    }
}

fn names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut names: Vec<_> = entries
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Masks elapsed times in snapshots taken while the guard lives, since they vary between runs.
fn mask_elapsed() -> impl Drop {
    let mut settings = Settings::clone_current();
    settings.add_filter(r"Duration: \d+\.\d s\.", "Duration: [elapsed] s.");
    settings.bind_to_scope()
}

#[test]
fn writes_a_line_per_stretch_of_speech() {
    let _masked = mask_elapsed();
    let workspace = Workspace::with("speech-bursts.mkv");

    assert_snapshot!(workspace.vt(&["speech-bursts.mkv"]));
    assert_eq!(
        fs::read_to_string(workspace.path("speech-bursts.srt")).unwrap(),
        BURSTS_SRT
    );
    assert_eq!(
        workspace.files(),
        ["speech-bursts.mkv", "speech-bursts.srt"]
    );
    assert_eq!(workspace.scratch(), Vec::<String>::new());
}

#[test]
fn keeps_an_existing_transcript() {
    let workspace = Workspace::with("speech-bursts.mkv");
    fs::write(workspace.path("speech-bursts.srt"), "edited").unwrap();

    assert_snapshot!(workspace.vt(&["speech-bursts.mkv"]));
    assert_eq!(
        fs::read_to_string(workspace.path("speech-bursts.srt")).unwrap(),
        "edited"
    );
}

#[test]
fn force_replaces_an_existing_transcript() {
    let workspace = Workspace::with("speech-bursts.mkv");
    fs::write(workspace.path("speech-bursts.srt"), "edited").unwrap();

    workspace.vt(&["speech-bursts.mkv", "--force"]);

    assert_eq!(
        fs::read_to_string(workspace.path("speech-bursts.srt")).unwrap(),
        BURSTS_SRT
    );
    assert_eq!(
        workspace.files(),
        ["speech-bursts.mkv", "speech-bursts.srt"]
    );
}

#[test]
fn fails_without_an_audio_stream() {
    let workspace = Workspace::with("chapters-valid.mkv");

    assert_snapshot!(workspace.vt(&["chapters-valid.mkv"]));
    assert_eq!(workspace.files(), ["chapters-valid.mkv"]);
}

#[test]
fn fails_on_a_missing_file() {
    assert_snapshot!(common::vt(&[
        "transcribe",
        "--backend",
        "fake",
        "missing.mp4"
    ]));
}

/// Waits until `vt-ml` is mid-run, reading the audio in its scratch directory, and returns it.
fn wait_for_scratch(workspace: &Workspace) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let scratch = workspace
            .scratch()
            .pop()
            .map(|name| workspace.cache().join(name));
        if let Some(scratch) = scratch
            && !common::processes_mentioning(&["vt-ml", scratch.to_str().unwrap()]).is_empty()
        {
            return scratch.to_str().unwrap().to_string();
        }
        assert!(Instant::now() < deadline, "vt-ml never started");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn transcribes_each_file_and_carries_on_past_a_failure() {
    let _masked = mask_elapsed();
    let workspace = Workspace::with("speech-bursts.mkv");
    fs::copy(
        workspace.path("speech-bursts.mkv"),
        workspace.path("two.mkv"),
    )
    .unwrap();

    assert_snapshot!(workspace.vt(&["speech-bursts.mkv", "missing.mkv", "two.mkv"]));
    assert_eq!(
        (
            fs::read_to_string(workspace.path("speech-bursts.srt")).unwrap(),
            fs::read_to_string(workspace.path("two.srt")).unwrap()
        ),
        (BURSTS_SRT.to_string(), BURSTS_SRT.to_string())
    );
}

#[test]
fn an_interrupt_skips_the_files_not_yet_started() {
    let workspace = Workspace::with("speech-bursts.mkv");
    fs::copy(
        workspace.path("speech-bursts.mkv"),
        workspace.path("two.mkv"),
    )
    .unwrap();
    let mut command = workspace.command("fake", &["speech-bursts.mkv", "two.mkv"]);
    let child = common::interruptible(&mut command)
        .env(FAKE_DELAY_ENV, "0.5")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_scratch(&workspace);

    common::interrupt(&child);
    let output = child.wait_with_output().unwrap();

    assert_snapshot!(common::render(&output));
    assert_eq!(workspace.files(), ["speech-bursts.mkv", "two.mkv"]);
    assert_eq!(workspace.scratch(), Vec::<String>::new());
}

#[test]
fn cancels_on_interrupt_and_cleans_up() {
    let workspace = Workspace::with("speech-bursts.mkv");
    let mut command = workspace.command("fake", &["speech-bursts.mkv"]);
    let child = common::interruptible(&mut command)
        .env(FAKE_DELAY_ENV, "0.5")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let scratch = wait_for_scratch(&workspace);

    common::interrupt(&child);
    let output = child.wait_with_output().unwrap();

    assert_snapshot!(common::render(&output));
    assert_eq!(workspace.scratch(), Vec::<String>::new());
    assert_eq!(workspace.files(), ["speech-bursts.mkv"]);
    assert_eq!(common::processes_mentioning(&[&scratch]), Vec::<u32>::new());
}

/// Whisper on synthetic speech: every line must lie within a sentence and hold its words.
#[test]
#[ignore = "needs the ML stack and a GPU"]
fn whisper_transcribes_speech() {
    let workspace = Workspace::with("speech-bursts.mkv");
    let speech = workspace.path("speech.mkv");
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(
            "flite=text='The quick brown fox jumps over the lazy dog.':voice=slt,\
             adelay=3000,apad=whole_dur=12",
        )
        .args(["-c:a", "flac"])
        .arg(&speech)
        .status()
        .unwrap();
    assert!(status.success());

    let output = workspace
        .command("whisper", &["speech.mkv"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", common::render(&output));
    let srt = fs::read_to_string(workspace.path("speech.srt")).unwrap();
    assert!(
        srt.starts_with("1\n00:00:03,") && srt.contains("quick brown fox"),
        "{srt}"
    );
}
