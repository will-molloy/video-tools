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
use insta::{Settings, assert_snapshot};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
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

/// The user's Hugging Face home, which holds their login and downloaded models. Hugging Face
/// otherwise looks under `XDG_CACHE_HOME`, which each test points at its own empty directory.
fn hf_home() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache"),
        PathBuf::from,
    );
    std::env::var_os("HF_HOME").map_or_else(|| cache.join("huggingface"), PathBuf::from)
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
    settings.add_filter(r"in \d+\.\d s\.", "in [elapsed] s.");
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

/// PIDs of processes whose command line mentions every one of `needles`.
fn processes_mentioning(needles: &[&str]) -> Vec<i32> {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmd| {
                let cmd = String::from_utf8_lossy(&cmd);
                needles.iter().all(|needle| cmd.contains(needle))
            })
        })
        .collect()
}

#[test]
fn cancels_on_sigint_and_cleans_up() {
    let workspace = Workspace::with("speech-bursts.mkv");
    let child = workspace
        .command("fake", &["speech-bursts.mkv"])
        .env(FAKE_DELAY_ENV, "0.5")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Waits until the stage is mid-run, reading the audio in its scratch directory.
    let deadline = Instant::now() + Duration::from_secs(60);
    let scratch = loop {
        let scratch = workspace
            .scratch()
            .pop()
            .map(|name| workspace.cache().join(name));
        if let Some(scratch) = scratch
            && !processes_mentioning(&["vt-ml", scratch.to_str().unwrap()]).is_empty()
        {
            break scratch.to_str().unwrap().to_string();
        }
        assert!(Instant::now() < deadline, "the stage never started");
        thread::sleep(Duration::from_millis(20));
    };

    kill(
        Pid::from_raw(child.id().try_into().unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_snapshot!(common::render(&output));
    assert_eq!(workspace.scratch(), Vec::<String>::new());
    assert_eq!(workspace.files(), ["speech-bursts.mkv"]);
    assert_eq!(processes_mentioning(&[&scratch]), Vec::<i32>::new());
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
