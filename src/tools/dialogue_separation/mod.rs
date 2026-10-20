//! `vt dialogue-separation`: separate dialogue from everything else and keep one side.
//!
//! The pipeline extracts the audio at the model's sample rate, runs the Python stage in `ml/`
//! (see its README for the contract), caches both stems, then writes the kept one: as a WAV
//! for audio-only inputs, or remuxed with the original video for the rest.

mod cache;
mod media;
mod speech;

use std::{
    fmt, fs,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{
    ffmpeg,
    ffprobe::{self, Cancelled, StreamKind},
    ml::{
        self, child,
        protocol::{Message, Stats},
    },
    srt,
    tools::{self, sync_check::thousands, transcribe},
};
use cache::{Cache, Entry, Key, Pending};
use media::{AudioStream, Encoding, Output};
use speech::Span;

#[derive(Debug, Args)]
pub struct DialogueSeparationArgs {
    /// Video or audio file to separate
    file: PathBuf,

    /// Which side of the separation to keep
    #[arg(long, value_enum)]
    keep: Keep,

    /// Separation model
    #[arg(long, value_enum, default_value_t = Backend::Tiger)]
    backend: Backend,

    /// Transcript of the speech to separate, from `vt transcribe`. Defaults to FILE with an
    /// `.srt` extension.
    #[arg(long)]
    transcript: Option<PathBuf>,

    /// Replace an existing output
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Keep {
    /// Dialogue only
    Voice,

    /// Everything but dialogue
    Ambience,
}

impl Keep {
    fn name(self) -> &'static str {
        match self {
            Keep::Voice => "voice",
            Keep::Ambience => "ambience",
        }
    }

    fn stem(self) -> &'static str {
        match self {
            Keep::Voice => cache::VOICE,
            Keep::Ambience => cache::AMBIENCE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    /// Meta SAM Audio, on the GPU
    Sam,

    /// TIGER-DnR, on the GPU
    Tiger,

    /// A deterministic stand-in for tests
    #[value(hide = true)]
    Fake,
}

impl Backend {
    /// The backends users can pick, the default first.
    pub const CHOICES: [Backend; 2] = [Backend::Tiger, Backend::Sam];

    /// The name the Python stage knows the backend by.
    fn name(self) -> &'static str {
        match self {
            Backend::Sam => "sam",
            Backend::Tiger => "tiger",
            Backend::Fake => "fake",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Backend::Sam => "SAM Audio",
            Backend::Tiger => "TIGER-DnR",
            Backend::Fake => "the fake backend",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Backend::Sam => "Needs 16 GB of VRAM and Hugging Face access",
            Backend::Tiger => "Fast, needs 2 GB of VRAM",
            Backend::Fake => "A deterministic stand-in for tests",
        }
    }

    /// The model's native rate, which the extracted audio must match.
    fn sample_rate(self) -> u32 {
        match self {
            Backend::Sam => 48_000,
            Backend::Tiger | Backend::Fake => 44_100,
        }
    }

    fn device(self) -> &'static str {
        match self {
            Backend::Sam | Backend::Tiger => "cuda",
            Backend::Fake => "cpu",
        }
    }

    /// The `ml/pyproject.toml` extra holding the model's dependencies.
    fn extra(self) -> Option<&'static str> {
        match self {
            Backend::Sam => Some("sam"),
            Backend::Tiger => Some("tiger"),
            Backend::Fake => None,
        }
    }
}

pub async fn cli(args: &DialogueSeparationArgs) -> Result<ExitCode> {
    let cancel = CancellationToken::new();
    let transcript = args
        .transcript
        .clone()
        .unwrap_or_else(|| transcribe::transcript_path(&args.file));
    let run = run(
        &args.file,
        &transcript,
        args.keep,
        args.backend,
        args.force,
        &cancel,
        |progress| ml::cli::show_progress(&progress),
    );
    let run = async { run.await.map_err(tools::suggest_force) };
    ml::cli::run(&cancel, run, |outcome| outcome.lines()).await
}

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Extracting,

    /// Waiting for the stage's first message. The first run of a backend also installs its
    /// Python dependencies here, which can take minutes.
    Starting,

    /// The stage's latest status, such as the model it's loading.
    Log(String),

    /// Counts one step per channel per chunk.
    Separating {
        done: u64,
        total: u64,
    },

    Writing,
}

impl Progress {
    /// The fraction done, while separating.
    #[allow(clippy::cast_precision_loss)]
    pub fn ratio(&self) -> Option<f64> {
        match *self {
            Progress::Separating { done, total } if total > 0 => {
                Some((done as f64 / total as f64).min(1.0))
            }
            _ => None,
        }
    }
}

impl fmt::Display for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Progress::Extracting => f.write_str("Extracting audio…"),
            Progress::Starting => f.write_str("Starting the separation stage…"),
            Progress::Log(message) => write!(f, "{message}…"),
            Progress::Separating { done, total } => {
                let percent = self.ratio().unwrap_or(0.0) * 100.0;
                write!(f, "{percent:.0}% · step {done} of {total}")
            }
            Progress::Writing => f.write_str("Writing the output…"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub keep: Keep,
    pub backend: Backend,
    pub output: PathBuf,
    pub stats: Stats,

    /// The speech intervals the model separated, and their total length.
    pub intervals: usize,
    pub speech: Duration,

    /// Whether the stems came from the cache instead of a new separation.
    pub cached: bool,
}

impl Outcome {
    pub fn lines(&self) -> Vec<String> {
        let file = self.output.file_name().map_or_else(
            || self.output.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let Stats {
            device,
            elapsed_s,
            peak_vram_mb,
        } = &self.stats;
        let backend = self.backend.label();
        let mut lines = vec![format!("Kept the {} in {file}.", self.keep.name())];
        lines.push(if self.cached {
            format!(
                "Reused the cached separation by {backend}, made on {device} in {elapsed_s:.1} s."
            )
        } else {
            format!("Separated with {backend} on {device} in {elapsed_s:.1} s.")
        });
        lines.push(match self.intervals {
            0 => "The transcript has no speech, so the audio is unchanged.".into(),
            1 => format!(
                "1 speech interval, {:.1} s in all.",
                self.speech.as_secs_f64()
            ),
            n => format!(
                "{n} speech intervals, {:.1} s in all.",
                self.speech.as_secs_f64()
            ),
        });
        if let Some(mb) = peak_vram_mb {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            lines.push(format!("Peak VRAM: {} MB.", thousands(mb.round() as u64)));
        }
        lines
    }
}

/// Separates the speech in `transcript` from the first audio stream of `path` and writes the kept
/// side next to it.
pub async fn run(
    path: &Path,
    transcript: &Path,
    keep: Keep,
    backend: Backend,
    force: bool,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(Progress),
) -> Result<Outcome> {
    let info = ffprobe::streams(path).await?;
    let audio = info
        .streams
        .iter()
        .find(|s| matches!(s.kind, StreamKind::Audio { .. }))
        .context("no audio stream")?;
    let StreamKind::Audio { sample_rate, .. } = audio.kind else {
        unreachable!("audio was found by kind");
    };
    let has_video = info
        .streams
        .iter()
        .any(|s| matches!(s.kind, StreamKind::Video { .. }));
    let output_file = output_path(path, keep, has_video);
    tools::check_output(&output_file, force)?;
    let spans = speech::spans(&read_transcript(path, transcript)?);

    // Decided before separating, which can take an hour, so a problem here fails fast.
    let output = if has_video {
        Output::Remux {
            path: output_file,
            audio: AudioStream {
                index: audio.index,
                sample_rate,
                start: audio.start.unwrap_or_default(),
            },
            encoding: Encoding::for_stream(path, audio).await?,
        }
    } else {
        Output::Wav {
            path: output_file,
            sample_rate,
        }
    };

    let cache = Cache::new()?;
    let key = Key::of(path, backend, &spans)?;
    let (entry, cached) = match cache.get(&key) {
        Some(entry) => (entry, true),
        None => {
            let pending = cache.begin(&key)?;
            let entry = separate(
                path,
                audio,
                backend,
                &spans,
                pending,
                cancel,
                &mut on_progress,
            )
            .await?;
            (entry, false)
        }
    };

    on_progress(Progress::Writing);
    cancel
        .run_until_cancelled(media::write(path, &entry.stem(keep.stem()), &output))
        .await
        .ok_or(Cancelled)??;
    Ok(Outcome {
        keep,
        backend,
        output: output.path().to_owned(),
        stats: entry.stats,
        intervals: spans.len(),
        speech: speech::duration(&spans, info.duration),
        cached,
    })
}

/// The transcript's lines. It's required, since the models only see the speech it marks.
fn read_transcript(file: &Path, transcript: &Path) -> Result<Vec<srt::Block>> {
    let text = fs::read_to_string(transcript).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow!(
            "no transcript at {}; run `vt transcribe {}` first",
            transcript.display(),
            file.display()
        ),
        _ => anyhow!("failed to read {}: {e}", transcript.display()),
    })?;
    srt::parse(&text).with_context(|| format!("failed to read {}", transcript.display()))
}

async fn separate(
    path: &Path,
    audio: &ffprobe::Stream,
    backend: Backend,
    spans: &[Span],
    pending: Pending,
    cancel: &CancellationToken,
    on_progress: &mut impl FnMut(Progress),
) -> Result<Entry> {
    // Dropping `pending` on any early return, including cancellation, deletes the partial entry.
    let input = pending.dir().join("input.wav");
    let speech = pending.dir().join("speech.json");
    fs::write(&speech, speech::json(spans))
        .with_context(|| format!("failed to write {}", speech.display()))?;
    // Built first, so a missing GPU fails before extraction.
    let mut command = ml::command("separate", backend.extra())?;
    command
        .arg("--input")
        .arg(&input)
        .arg("--output-dir")
        .arg(pending.dir())
        .arg("--speech")
        .arg(&speech)
        .args(["--backend", backend.name(), "--device", backend.device()]);

    on_progress(Progress::Extracting);
    cancel
        .run_until_cancelled(ffmpeg::extract_audio(
            path,
            audio.index,
            backend.sample_rate(),
            None,
            &input,
        ))
        .await
        .ok_or(Cancelled)??;

    on_progress(Progress::Starting);
    let done = child::run::<Stems>(
        command,
        cancel,
        child::TERM_GRACE,
        |message| match message {
            Message::Progress { done, total } => on_progress(Progress::Separating { done, total }),
            Message::Log { message } => on_progress(Progress::Log(message)),
            Message::Done(_) | Message::Error { .. } => {}
        },
    )
    .await?;

    let Stems { voice, ambience } = &done.result;
    let expected = (
        pending.dir().join(cache::VOICE),
        pending.dir().join(cache::AMBIENCE),
    );
    if (voice, ambience) != (&expected.0, &expected.1) {
        bail!(
            "the separate stage wrote {} and {}, not the expected {} and {}",
            voice.display(),
            ambience.display(),
            expected.0.display(),
            expected.1.display()
        );
    }
    pending.commit(&done.stats)
}

#[derive(Debug, Deserialize)]
struct Stems {
    voice: PathBuf,
    ambience: PathBuf,
}

/// `talk.mp4` becomes `talk.voice.mp4`, or `talk.voice.wav` for audio-only inputs.
fn output_path(input: &Path, keep: Keep, has_video: bool) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let ext = if has_video {
        input
            .extension()
            .map_or_else(|| "mkv".into(), |e| e.to_string_lossy())
    } else {
        "wav".into()
    };
    input.with_file_name(format!("{stem}.{}.{ext}", keep.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_video_outputs_after_the_input() {
        assert_eq!(
            output_path(Path::new("/v/talk.mp4"), Keep::Voice, true),
            Path::new("/v/talk.voice.mp4")
        );
    }

    #[test]
    fn names_audio_only_outputs_as_wav() {
        assert_eq!(
            output_path(Path::new("/v/talk.m4a"), Keep::Ambience, false),
            Path::new("/v/talk.ambience.wav")
        );
    }

    #[test]
    fn describes_separation_progress() {
        let progress = Progress::Separating { done: 3, total: 8 };

        assert_eq!(progress.to_string(), "38% · step 3 of 8");
        assert_eq!(progress.ratio(), Some(0.375));
    }

    #[test]
    fn describes_stage_logs() {
        assert_eq!(
            Progress::Log("Loading facebook/sam-audio-large".into()).to_string(),
            "Loading facebook/sam-audio-large…"
        );
    }

    fn outcome(cached: bool, peak_vram_mb: Option<f64>) -> Outcome {
        Outcome {
            keep: Keep::Voice,
            backend: Backend::Sam,
            output: "/v/talk.voice.mp4".into(),
            stats: Stats {
                device: "cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2)".into(),
                elapsed_s: 83.24,
                peak_vram_mb,
            },
            intervals: 2,
            speech: Duration::from_millis(5250),
            cached,
        }
    }

    #[test]
    fn reports_a_fresh_separation_with_vram() {
        assert_eq!(
            outcome(false, Some(9123.5)).lines(),
            [
                "Kept the voice in talk.voice.mp4.",
                "Separated with SAM Audio on cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2) in 83.2 s.",
                "2 speech intervals, 5.2 s in all.",
                "Peak VRAM: 9,124 MB.",
            ]
        );
    }

    #[test]
    fn reports_a_cache_hit() {
        assert_eq!(
            outcome(true, None).lines(),
            [
                "Kept the voice in talk.voice.mp4.",
                "Reused the cached separation by SAM Audio, made on \
                 cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2) in 83.2 s.",
                "2 speech intervals, 5.2 s in all.",
            ]
        );
    }

    #[test]
    fn reports_one_interval_and_none() {
        let with = |intervals, speech_ms| {
            let outcome = Outcome {
                intervals,
                speech: Duration::from_millis(speech_ms),
                ..outcome(true, None)
            };
            outcome.lines()[2].clone()
        };

        assert_eq!(with(1, 3000), "1 speech interval, 3.0 s in all.");
        assert_eq!(
            with(0, 0),
            "The transcript has no speech, so the audio is unchanged."
        );
    }
}
