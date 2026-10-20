//! Runs `vt dialogue-separation` with the fake backend, which needs no ML dependencies, and
//! checks the files it writes with ffprobe and ffmpeg.
//!
//! The fake backend's voice is the input and its ambience is the input at a quarter gain, so
//! decoding the output and comparing it with the input checks which stem was kept and that the
//! audio stayed in sync.
//!
//! The `#[ignore]`d tests at the end run the real backends on a synthetic clip and need the ML
//! stack and a GPU. `scripts/real-backends.sh` runs them.

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

/// The user's Hugging Face home, which holds their login and downloaded models. Hugging Face
/// otherwise looks under `XDG_CACHE_HOME`, which each test points at its own empty directory.
fn hf_home() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache"),
        PathBuf::from,
    );
    std::env::var_os("HF_HOME").map_or_else(|| cache.join("huggingface"), PathBuf::from)
}

const FAKE_AMBIENCE_GAIN: f32 = 0.25;
const FAKE_DELAY_ENV: &str = "VT_ML_FAKE_DELAY_S";

/// A scratch directory holding a copy of a fixture and an empty cache.
struct Workspace {
    dir: TempDir,
}

/// A transcript with one line covering any file.
const WHOLE_FILE_SRT: &str = "1\n00:00:00,000 --> 01:00:00,000\nspeech\n\n";

impl Workspace {
    /// A workspace holding `fixture` and a transcript that covers all of it.
    fn transcribed(fixture: &str) -> Workspace {
        let workspace = Workspace::with(fixture);
        workspace.write_transcript(fixture, WHOLE_FILE_SRT);
        workspace
    }

    /// Writes the default transcript of `fixture`, such as `talk.srt` for `talk.mp4`.
    fn write_transcript(&self, fixture: &str, srt: &str) {
        fs::write(self.path(&fixture_srt(fixture)), srt).unwrap();
    }

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
        self.path("cache").join("video-tools/dialogue-separation")
    }

    fn command(&self, args: &[&str]) -> Command {
        self.command_with("fake", args)
    }

    fn command_with(&self, backend: &str, args: &[&str]) -> Command {
        // A std `Command`, unlike assert_cmd's, can spawn without waiting, for the cancel test.
        let mut command = Command::new(cargo_bin!("vt"));
        command
            .current_dir(self.dir.path())
            .env("HF_HOME", hf_home())
            .env("XDG_CACHE_HOME", self.path("cache"))
            .args(["dialogue-separation", "--backend", backend])
            .args(args);
        command
    }

    fn vt(&self, args: &[&str]) -> String {
        common::render(&self.command(args).output().unwrap())
    }

    /// File names in the workspace, excluding the cache.
    fn files(&self) -> Vec<String> {
        names(self.dir.path())
            .into_iter()
            .filter(|name| name != "cache")
            .collect()
    }

    /// Entry names in the cache, including partial ones.
    fn cache_entries(&self) -> Vec<String> {
        names(&self.cache())
    }
}

fn fixture_srt(fixture: &str) -> String {
    format!(
        "{}.srt",
        Path::new(fixture).file_stem().unwrap().to_str().unwrap()
    )
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

fn probe(path: &Path, stream: &str, entries: &str) -> String {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            stream,
            "-show_entries",
            entries,
        ])
        .args(["-of", "default=nw=1:nk=1"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Decodes a file's first audio stream to interleaved float samples at 48 kHz, placed by their
/// timestamps the way a player places them.
fn decode(path: &Path) -> (Vec<f32>, u16) {
    let channels = probe(path, "a:0", "stream=channels").parse().unwrap();
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-af", "aresample=async=1:first_pts=0"])
        .args(["-ar", "48000", "-f", "f32le", "-"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let samples = output
        .stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&b| f32::from_le_bytes(b))
        .collect();
    (samples, channels)
}

/// MP4 stores a stream's start delay in its 1 kHz movie timescale, so starts can move by up to
/// a millisecond.
const SYNC_TOLERANCE_FRAMES: usize = 48;

/// Asserts `actual`'s audio is `expected`'s scaled by `gain` and in sync with it, give or take
/// lossy coding. Audio that is shifted, has swapped channels, or is the wrong stem differs by
/// about its own level.
#[allow(clippy::cast_precision_loss)]
fn assert_audio_matches(actual: &Path, expected: &Path, gain: f32) {
    let ((actual, actual_channels), (expected, expected_channels)) =
        (decode(actual), decode(expected));
    assert_eq!(actual_channels, expected_channels);
    let channels = usize::from(expected_channels);
    // Skips the edges, where resampling and codec priming smear.
    let margin = 4800 * channels;
    let (from, to) = (margin, expected.len().min(actual.len()) - margin);
    let rms = |f: &dyn Fn(usize) -> f32| {
        ((from..to).map(|i| f(i).powi(2)).sum::<f32>() / (to - from) as f32).sqrt()
    };
    let level = rms(&|i| expected[i] * gain);
    // Shifting by whole frames keeps channels lined up with themselves.
    let step = isize::try_from(channels).unwrap();
    let error_at =
        |frames: isize| rms(&|i| actual[i.strict_add_signed(frames * step)] - expected[i] * gain);
    let tolerance = isize::try_from(SYNC_TOLERANCE_FRAMES).unwrap();
    let (frames, error) = (-tolerance..=tolerance)
        .map(|frames| (frames, error_at(frames)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap();
    assert!(
        error < level * 0.05,
        "output differs from the input by {error} at best ({frames} frames off), against a \
         level of {level}"
    );
}

/// Hashes a file's video packets, which `-c:v copy` must leave bit-identical.
fn video_hash(path: &Path) -> String {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v",
            "-c",
            "copy",
            "-f",
            "streamhash",
            "-hash",
            "sha256",
            "-",
        ])
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap()
}

/// Bits per second over a stream's packets, which works whether or not the container records
/// a bit rate.
fn measured_bit_rate(path: &Path) -> f64 {
    let rows = probe(path, "a:0", "packet=size,duration_time");
    let values: Vec<f64> = rows.lines().map(|v| v.parse().unwrap_or(0.0)).collect();
    let (bytes, secs) = values
        .as_chunks::<2>()
        .0
        .iter()
        .fold((0.0, 0.0), |(b, s), [size, duration]| {
            (b + size, s + duration)
        });
    bytes * 8.0 / secs
}

fn assert_close(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= expected * tolerance,
        "{actual} is not within {}% of {expected}",
        tolerance * 100.0
    );
}

/// Asserts the remux kept the video bit for bit and matched the source audio's encoding.
fn assert_remuxed(output: &Path, source: &Path) {
    assert_eq!(video_hash(output), video_hash(source));
    let encoding = "stream=codec_name,sample_rate,channels";
    assert_eq!(probe(output, "a", encoding), probe(source, "a", encoding));
    // An encoder hits its target bit rate only on average.
    assert_close(measured_bit_rate(output), measured_bit_rate(source), 0.03);
}

#[test]
fn keep_voice_remuxes_the_video() {
    let _mask = mask_elapsed();
    let workspace = Workspace::transcribed("clean-cfr.mp4");

    assert_snapshot!(workspace.vt(&["clean-cfr.mp4", "--keep", "voice"]));

    let source = workspace.path("clean-cfr.mp4");
    let out = workspace.path("clean-cfr.voice.mp4");
    assert_eq!(
        workspace.files(),
        ["clean-cfr.mp4", "clean-cfr.srt", "clean-cfr.voice.mp4"]
    );
    assert_remuxed(&out, &source);
    assert_audio_matches(&out, &source, 1.0);
}

#[test]
fn keep_ambience_remuxes_the_video() {
    let _mask = mask_elapsed();
    let workspace = Workspace::transcribed("clean-cfr.mp4");

    assert_snapshot!(workspace.vt(&["clean-cfr.mp4", "--keep", "ambience"]));

    let source = workspace.path("clean-cfr.mp4");
    let out = workspace.path("clean-cfr.ambience.mp4");
    assert_remuxed(&out, &source);
    assert_audio_matches(&out, &source, FAKE_AMBIENCE_GAIN);
}

#[test]
fn remuxes_matroska_without_a_recorded_bit_rate() {
    let _mask = mask_elapsed();
    let workspace = Workspace::transcribed("clean-cfr.mkv");
    let source = workspace.path("clean-cfr.mkv");
    // Matroska files ffmpeg writes record no bit rate, so vt measures it.
    assert_eq!(
        probe(&source, "a", "stream=bit_rate:stream_tags=BPS"),
        "N/A"
    );

    assert_snapshot!(workspace.vt(&["clean-cfr.mkv", "--keep", "voice"]));

    assert_remuxed(&workspace.path("clean-cfr.voice.mkv"), &source);
}

#[test]
fn keeps_the_audio_start_offset() {
    let workspace = Workspace::transcribed("audio-offset.mp4");
    let source = workspace.path("audio-offset.mp4");

    workspace.vt(&["audio-offset.mp4", "--keep", "voice"]);

    let out = workspace.path("audio-offset.voice.mp4");
    let start = |path: &Path| {
        probe(path, "a", "stream=start_time")
            .parse::<f64>()
            .unwrap()
    };
    assert_close(start(&out), start(&source), 0.05);
    assert_audio_matches(&out, &source, 1.0);
}

#[test]
fn writes_a_wav_for_audio_only_input() {
    let _mask = mask_elapsed();
    let workspace = Workspace::transcribed("stereo-tone.m4a");

    assert_snapshot!(workspace.vt(&["stereo-tone.m4a", "--keep", "voice"]));

    let out = workspace.path("stereo-tone.voice.wav");
    assert_eq!(
        workspace.files(),
        [
            "stereo-tone.m4a",
            "stereo-tone.srt",
            "stereo-tone.voice.wav"
        ]
    );
    assert_eq!(
        probe(&out, "a", "stream=codec_name,sample_rate,channels"),
        "pcm_f32le\n48000\n2"
    );
    // The comparison runs over interleaved samples, so it also catches swapped channels.
    assert_audio_matches(&out, &workspace.path("stereo-tone.m4a"), 1.0);
}

#[test]
fn reuses_cached_stems_without_running_the_model() {
    let _mask = mask_elapsed();
    let workspace = Workspace::transcribed("clean-cfr.mp4");
    workspace.vt(&["clean-cfr.mp4", "--keep", "voice"]);

    // With no Python project to run, only a cache hit can succeed.
    let output = workspace
        .command(&["clean-cfr.mp4", "--keep", "ambience"])
        .env("VT_ML_PROJECT", workspace.path("no-such-project"))
        .output()
        .unwrap();

    assert_snapshot!(common::render(&output));
    assert_audio_matches(
        &workspace.path("clean-cfr.ambience.mp4"),
        &workspace.path("clean-cfr.mp4"),
        FAKE_AMBIENCE_GAIN,
    );
}

#[test]
fn separates_again_after_the_file_changes() {
    let _mask = mask_elapsed();
    let workspace = Workspace::transcribed("clean-cfr.mp4");
    workspace.vt(&["clean-cfr.mp4", "--keep", "voice"]);
    fs::copy(
        Path::new(common::FIXTURES).join("audio-offset.mp4"),
        workspace.path("clean-cfr.mp4"),
    )
    .unwrap();

    assert_snapshot!(workspace.vt(&["clean-cfr.mp4", "--keep", "voice", "--force"]));

    assert_eq!(workspace.cache_entries().len(), 2);
}

#[test]
fn keeps_an_existing_output() {
    let workspace = Workspace::transcribed("clean-cfr.mp4");
    fs::write(workspace.path("clean-cfr.voice.mp4"), "mine").unwrap();

    assert_snapshot!(workspace.vt(&["clean-cfr.mp4", "--keep", "voice"]));

    assert_eq!(
        fs::read_to_string(workspace.path("clean-cfr.voice.mp4")).unwrap(),
        "mine"
    );
    assert_eq!(workspace.cache_entries(), Vec::<String>::new());
}

/// PIDs of processes whose command line mentions `needle`.
fn processes_mentioning(needle: &str) -> Vec<i32> {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmd| String::from_utf8_lossy(&cmd).contains(needle))
        })
        .collect()
}

#[test]
fn cancels_on_sigint_and_cleans_up() {
    let workspace = Workspace::transcribed("clean-cfr.mp4");
    let child = workspace
        .command(&["clean-cfr.mp4", "--keep", "voice"])
        .env(FAKE_DELAY_ENV, "0.5")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Waits until the stage is mid-run, with its stems open in a partial cache entry.
    let deadline = Instant::now() + Duration::from_secs(60);
    let partial = loop {
        let partial = workspace
            .cache_entries()
            .into_iter()
            .map(|name| workspace.cache().join(name))
            .find(|dir| dir.join("voice.wav").exists());
        if let Some(partial) = partial {
            break partial.to_str().unwrap().to_string();
        }
        assert!(Instant::now() < deadline, "the stage never started");
        thread::sleep(Duration::from_millis(20));
    };
    // uv and Python, which both name the partial entry in their arguments.
    assert!(!processes_mentioning(&partial).is_empty());

    kill(
        Pid::from_raw(child.id().try_into().unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_snapshot!(common::render(&output));
    assert_eq!(workspace.cache_entries(), Vec::<String>::new());
    assert_eq!(workspace.files(), ["clean-cfr.mp4", "clean-cfr.srt"]);
    assert_eq!(processes_mentioning(&partial), Vec::<i32>::new());
}

#[test]
fn fails_without_a_transcript() {
    let workspace = Workspace::with("clean-cfr.mp4");

    assert_snapshot!(workspace.vt(&["clean-cfr.mp4", "--keep", "voice"]));

    assert_eq!(workspace.files(), ["clean-cfr.mp4"]);
    assert_eq!(workspace.cache_entries(), Vec::<String>::new());
}

#[test]
fn fails_on_a_malformed_transcript() {
    let workspace = Workspace::with("clean-cfr.mp4");
    workspace.write_transcript("clean-cfr.mp4", "1\nsoon --> later\nhello\n");

    assert_snapshot!(workspace.vt(&["clean-cfr.mp4", "--keep", "voice"]));
}

#[test]
fn reads_a_transcript_from_another_path() {
    let workspace = Workspace::with("stereo-tone.m4a");
    fs::write(workspace.path("lines.srt"), WHOLE_FILE_SRT).unwrap();

    workspace.vt(&[
        "stereo-tone.m4a",
        "--keep",
        "voice",
        "--transcript",
        "lines.srt",
    ]);

    assert_audio_matches(
        &workspace.path("stereo-tone.voice.wav"),
        &workspace.path("stereo-tone.m4a"),
        1.0,
    );
}

/// RMS of a file's first audio stream between two times, over all channels.
#[allow(clippy::cast_precision_loss)]
fn rms_between(path: &Path, from_secs: f64, to_secs: f64) -> f64 {
    let (samples, channels) = decode(path);
    let at = |secs: f64| (secs * 48_000.0) as usize * usize::from(channels);
    let window = &samples[at(from_secs)..at(to_secs)];
    (window.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / window.len() as f64).sqrt()
}

#[test]
fn separates_only_the_speech_in_the_transcript() {
    let _mask = mask_elapsed();
    let workspace = Workspace::with("speech-bursts.mkv");
    // The fixture has tone bursts at 1-2 s and 3.5-4.5 s. Only the first is marked as speech.
    workspace.write_transcript(
        "speech-bursts.mkv",
        "1\n00:00:01,000 --> 00:00:02,000\nspeech\n\n",
    );
    let source = workspace.path("speech-bursts.mkv");
    let level = rms_between(&source, 1.3, 1.7);

    assert_snapshot!(workspace.vt(&["speech-bursts.mkv", "--keep", "voice"]));
    workspace.vt(&["speech-bursts.mkv", "--keep", "ambience"]);

    let voice = workspace.path("speech-bursts.voice.mkv");
    let ambience = workspace.path("speech-bursts.ambience.mkv");
    // Inside the speech the fake model keeps the voice whole and the ambience at a quarter.
    assert_close(rms_between(&voice, 1.3, 1.7), level, 0.02);
    assert_close(rms_between(&ambience, 1.3, 1.7), level * 0.25, 0.02);
    // Outside it the voice is silent and the ambience is the original.
    assert!(rms_between(&voice, 3.6, 4.4) < 1e-4);
    assert_close(rms_between(&ambience, 3.6, 4.4), level, 0.02);
}

#[test]
fn separates_again_after_the_transcript_changes() {
    let _mask = mask_elapsed();
    let workspace = Workspace::with("speech-bursts.mkv");
    workspace.write_transcript(
        "speech-bursts.mkv",
        "1\n00:00:01,000 --> 00:00:02,000\nspeech\n\n",
    );
    workspace.vt(&["speech-bursts.mkv", "--keep", "voice"]);
    workspace.write_transcript(
        "speech-bursts.mkv",
        "1\n00:00:01,000 --> 00:00:02,000\nspeech\n\n\
         2\n00:00:03,500 --> 00:00:04,500\nmumbling\n\n",
    );

    assert_snapshot!(workspace.vt(&["speech-bursts.mkv", "--keep", "voice", "--force"]));

    assert_eq!(workspace.cache_entries().len(), 2);
}

#[test]
fn fails_without_an_audio_stream() {
    let workspace = Workspace::with("chapters-valid.mkv");

    assert_snapshot!(workspace.vt(&["chapters-valid.mkv", "--keep", "voice"]));
}

#[test]
fn fails_on_a_missing_file() {
    let workspace = Workspace::with("clean-cfr.mp4");

    assert_snapshot!(workspace.vt(&["missing.mp4", "--keep", "voice"]));
}

const CLIP_SECS: &str = "30";

const SPEECH: &str = "The quick brown fox jumps over the lazy dog. Separation models split a \
    recording into speech and everything else. Rain falls on the roof while a radio plays in \
    the next room. We keep talking so the model has plenty of dialogue to find. The clip runs \
    for half a minute, which covers several chunks.";

fn ffmpeg(args: &[&str]) {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Writes `clip.mp4`, a 30 s video whose stereo audio mixes synthesised speech with a bed of
/// brown noise and a chord at about the same level, plus `speech.wav` and `bed.wav` holding
/// the two parts. The noise differs between channels, so a mono shortcut would show.
fn synthetic_clip(workspace: &Workspace) {
    let speech =
        format!("flite=text='{SPEECH}':voice=slt,aresample=48000,apad=whole_dur={CLIP_SECS}");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &speech,
        "-t",
        CLIP_SECS,
        "-ac",
        "2",
        "-c:a",
        "pcm_f32le",
        workspace.path("speech.wav").to_str().unwrap(),
    ]);
    let noise = |seed: u8| {
        format!(
            "anoisesrc=color=brown:amplitude=0.2:sample_rate=48000:duration={CLIP_SECS}:seed={seed}"
        )
    };
    let chord = format!(
        "aevalsrc=0.04*(sin(2*PI*220*t)+sin(2*PI*277*t)+sin(2*PI*330*t)):s=48000:d={CLIP_SECS}"
    );
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &noise(1),
        "-f",
        "lavfi",
        "-i",
        &noise(2),
        "-f",
        "lavfi",
        "-i",
        &chord,
        "-filter_complex",
        "[0][1]amerge=inputs=2[n];[2]aformat=channel_layouts=stereo[c];\
         [n][c]amix=inputs=2:normalize=0",
        "-c:a",
        "pcm_f32le",
        workspace.path("bed.wav").to_str().unwrap(),
    ]);
    // Encoded like a typical render: H.264 with AAC at 320 kb/s.
    let video = format!("testsrc2=size=320x180:rate=30:duration={CLIP_SECS}");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &video,
        "-i",
        workspace.path("speech.wav").to_str().unwrap(),
        "-i",
        workspace.path("bed.wav").to_str().unwrap(),
        "-filter_complex",
        "[1][2]amix=inputs=2:normalize=0[a]",
        "-map",
        "0:v",
        "-map",
        "[a]",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-c:a",
        "aac",
        "-b:a",
        "320k",
        workspace.path("clip.mp4").to_str().unwrap(),
    ]);
}

/// Scale-invariant signal-to-distortion ratio of `estimate` against `reference`, in dB.
fn si_sdr(estimate: &[f32], reference: &[f32]) -> f64 {
    let n = estimate.len().min(reference.len());
    let dot = |a: &[f32], b: &[f32]| -> f64 {
        a[..n]
            .iter()
            .zip(&b[..n])
            .map(|(x, y)| f64::from(*x) * f64::from(*y))
            .sum()
    };
    let scale = dot(estimate, reference) / dot(reference, reference);
    let (target, error) =
        reference[..n]
            .iter()
            .zip(&estimate[..n])
            .fold((0.0, 0.0), |(target, error), (&r, &e)| {
                let t = scale * f64::from(r);
                (target + t * t, error + (f64::from(e) - t).powi(2))
            });
    10.0 * (target / error).log10()
}

/// Separates the synthetic clip both ways, prints each run's report, and checks that each
/// kept side is closer to its clean part than the mix was.
fn separates_the_synthetic_clip(backend: &str) {
    let workspace = Workspace {
        dir: tempfile::tempdir().unwrap(),
    };
    synthetic_clip(&workspace);
    workspace.write_transcript("clip.mp4", WHOLE_FILE_SRT);

    // The second run reuses the first's cached stems.
    for keep in ["voice", "ambience"] {
        let output = workspace
            .command_with(backend, &["clip.mp4", "--keep", keep])
            .output()
            .unwrap();
        println!("{backend} --keep {keep}:\n{}", common::render(&output));
        assert!(output.status.success());
    }

    let (mix, _) = decode(&workspace.path("clip.mp4"));
    for (kept, part) in [("voice", "speech"), ("ambience", "bed")] {
        let (reference, _) = decode(&workspace.path(&format!("{part}.wav")));
        let (separated, _) = decode(&workspace.path(&format!("clip.{kept}.mp4")));
        let (before, after) = (si_sdr(&mix, &reference), si_sdr(&separated, &reference));
        println!(
            "{backend} {kept} SI-SDR against the clean {part}: {after:.1} dB, mix {before:.1} dB"
        );
        assert!(
            after > before + 3.0,
            "{kept} improved SI-SDR by only {:.1} dB",
            after - before
        );
    }
}

#[test]
#[ignore = "needs SAM Audio's checkpoint and a GPU; run scripts/real-backends.sh"]
fn sam_separates_the_synthetic_clip() {
    separates_the_synthetic_clip("sam");
}

#[test]
#[ignore = "needs TIGER-DnR's dependencies; run scripts/real-backends.sh"]
fn tiger_separates_the_synthetic_clip() {
    separates_the_synthetic_clip("tiger");
}
