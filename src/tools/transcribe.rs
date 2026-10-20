//! `vt transcribe`: write the speech in a file as an SRT transcript next to it.
//!
//! The pipeline extracts the first audio stream as mono 16 kHz, runs the `transcribe` stage in
//! `ml/` (see its README for the contract), then writes one SRT block per segment of speech.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{
    ffmpeg,
    ffprobe::{self, Cancelled, StreamKind},
    ml::{
        self, child,
        protocol::{Message, Stats},
        scratch::{self, Scratch},
    },
    quote::{quote, quote_name},
    srt::{self, Block},
    tools::{self, sync_check::thousands},
};

/// Whisper's rate, which the stage requires.
const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, Args)]
pub struct TranscribeArgs {
    /// Video or audio file to transcribe
    file: PathBuf,

    /// Replace an existing transcript, losing any edits to it
    #[arg(long)]
    force: bool,

    #[arg(long, value_enum, default_value_t = Backend::Whisper, hide = true)]
    backend: Backend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    /// Whisper large-v3-turbo, on the GPU
    Whisper,

    /// A deterministic stand-in for tests
    Fake,
}

impl Backend {
    /// The name the stage knows the backend by.
    fn name(self) -> &'static str {
        match self {
            Backend::Whisper => "whisper",
            Backend::Fake => "fake",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Backend::Whisper => "Whisper large-v3-turbo",
            Backend::Fake => "the fake backend",
        }
    }

    fn device(self) -> &'static str {
        match self {
            Backend::Whisper => "cuda",
            Backend::Fake => "cpu",
        }
    }

    /// The `ml/pyproject.toml` extra holding the model's dependencies.
    fn extra(self) -> Option<&'static str> {
        match self {
            Backend::Whisper => Some("whisper"),
            Backend::Fake => None,
        }
    }
}

pub async fn cli(args: &TranscribeArgs) -> Result<ExitCode> {
    let cancel = CancellationToken::new();
    let run = async {
        run(&args.file, args.backend, args.force, &cancel, |progress| {
            ml::cli::show_progress(&progress);
        })
        .await
        .map_err(tools::suggest_force)
    };
    ml::cli::run(&cancel, run, |outcome| outcome.lines()).await
}

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Extracting,

    /// Waiting for the stage's first message. The first run also installs the model's Python
    /// dependencies here, which can take minutes.
    Starting,

    /// The stage's latest status, such as the model it's loading.
    Log(String),

    Transcribing {
        done: Duration,
        total: Duration,
    },
}

impl Progress {
    /// The fraction done, while transcribing.
    pub fn ratio(&self) -> Option<f64> {
        match *self {
            Progress::Transcribing { done, total } if !total.is_zero() => {
                Some((done.as_secs_f64() / total.as_secs_f64()).min(1.0))
            }
            _ => None,
        }
    }
}

impl fmt::Display for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Progress::Extracting => f.write_str("Extracting audio…"),
            Progress::Starting => f.write_str("Starting the transcription stage…"),
            Progress::Log(message) => write!(f, "{message}…"),
            Progress::Transcribing { done, total } => {
                let percent = self.ratio().unwrap_or(0.0) * 100.0;
                write!(f, "{percent:.0}% · {} of {}", clock(*done), clock(*total))
            }
        }
    }
}

/// `M:SS`, or `H:MM:SS` from an hour.
fn clock(time: Duration) -> String {
    let secs = time.as_secs();
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub output: PathBuf,
    pub blocks: usize,
    pub backend: Backend,
    pub stats: Stats,
}

impl Outcome {
    pub fn lines(&self) -> Vec<String> {
        let file = quote_name(&self.output);
        let Stats {
            device,
            elapsed_s,
            peak_vram_mb,
        } = &self.stats;
        let mut lines = vec![match self.blocks {
            0 => format!("Found no speech, so {file} is empty."),
            1 => format!("Wrote 1 line of speech to {file}."),
            n => format!("Wrote {n} lines of speech to {file}."),
        }];
        lines.push(format!(
            "Transcribed with {} on {device}.",
            self.backend.label()
        ));
        lines.push(format!("Duration: {elapsed_s:.1} s."));
        if let Some(mb) = peak_vram_mb {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            lines.push(format!("Peak VRAM: {} MB.", thousands(mb.round() as u64)));
        }
        lines
    }
}

/// Transcribes the first audio stream of `path` into `<name>.srt` next to it.
pub async fn run(
    path: &Path,
    backend: Backend,
    force: bool,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(Progress),
) -> Result<Outcome> {
    let output = path.with_extension("srt");
    tools::check_output(&output, force)?;
    let info = ffprobe::streams(path).await?;
    let audio = info
        .streams
        .iter()
        .find(|s| matches!(s.kind, StreamKind::Audio { .. }))
        .context("no audio stream")?;

    // Dropping `scratch` on any early return, including cancellation, deletes it.
    let scratch = Scratch::new(&scratch::cache_dir("transcribe")?, "run")?;
    let input = scratch.dir().join("input.wav");
    // Built first, so a missing GPU fails before extraction.
    let mut command = ml::command("transcribe", backend.extra())?;
    command.arg("--input").arg(&input).args([
        "--backend",
        backend.name(),
        "--device",
        backend.device(),
    ]);

    on_progress(Progress::Extracting);
    cancel
        .run_until_cancelled(ffmpeg::extract_audio(
            path,
            audio.index,
            SAMPLE_RATE,
            Some(1),
            &input,
        ))
        .await
        .ok_or(Cancelled)??;

    on_progress(Progress::Starting);
    let done = child::run::<Transcript>(
        command,
        cancel,
        child::TERM_GRACE,
        |message| match message {
            Message::Progress { done, total } => on_progress(Progress::Transcribing {
                done: Duration::from_millis(done),
                total: Duration::from_millis(total),
            }),
            Message::Log { message } => on_progress(Progress::Log(message)),
            Message::Done(_) | Message::Error { .. } => {}
        },
    )
    .await?;

    let blocks: Vec<Block> = done.result.segments.into_iter().map(Block::from).collect();
    write(&output, &srt::format(&blocks))?;
    Ok(Outcome {
        output,
        blocks: blocks.len(),
        backend,
        stats: done.stats,
    })
}

#[derive(Debug, Deserialize)]
struct Transcript {
    segments: Vec<Segment>,
}

/// Seconds from the start of the audio.
#[derive(Debug, Deserialize)]
struct Segment {
    start: f64,
    end: f64,
    text: String,
}

impl From<Segment> for Block {
    fn from(segment: Segment) -> Block {
        Block {
            start: Duration::from_secs_f64(segment.start),
            end: Duration::from_secs_f64(segment.end),
            text: segment.text,
        }
    }
}

/// Writes beside `path`, then renames, so a crash never leaves half a transcript.
fn write(path: &Path, contents: &str) -> Result<()> {
    let partial = path.with_extension("srt.partial");
    fs::write(&partial, contents)
        .with_context(|| format!("failed to write {}", quote(&partial)))?;
    fs::rename(&partial, path).with_context(|| format!("failed to write {}", quote(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_transcription_progress() {
        let progress = Progress::Transcribing {
            done: Duration::from_millis(75_500),
            total: Duration::from_secs(3 * 3600),
        };

        assert_eq!(progress.to_string(), "1% · 1:15 of 3:00:00");
    }

    use crate::quote;

    #[test]
    fn describes_stage_logs() {
        assert_eq!(
            Progress::Log("Loading openai/whisper-large-v3-turbo".into()).to_string(),
            "Loading openai/whisper-large-v3-turbo…"
        );
    }

    fn outcome(blocks: usize, peak_vram_mb: Option<f64>) -> Outcome {
        Outcome {
            output: "/v/talk.srt".into(),
            blocks,
            backend: Backend::Whisper,
            stats: Stats {
                device: "cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2)".into(),
                elapsed_s: 9.75,
                peak_vram_mb,
            },
        }
    }

    /// The outcome's lines as the CLI prints them.
    fn printed(outcome: &Outcome) -> Vec<String> {
        outcome
            .lines()
            .iter()
            .map(|line| quote::plain(line))
            .collect()
    }

    #[test]
    fn reports_the_transcript_and_how_it_ran() {
        assert_eq!(
            printed(&outcome(12, Some(4752.0))),
            [
                "Wrote 12 lines of speech to \"talk.srt\".",
                "Transcribed with Whisper large-v3-turbo on cuda:0 (AMD Radeon RX 9070 XT, HIP \
                 7.2).",
                "Duration: 9.8 s.",
                "Peak VRAM: 4,752 MB.",
            ]
        );
    }

    #[test]
    fn reports_a_transcript_without_speech() {
        assert_eq!(
            printed(&outcome(0, None))[0],
            "Found no speech, so \"talk.srt\" is empty."
        );
    }
}
