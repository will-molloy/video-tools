//! `vt dialogue-separation`: separate dialogue from everything else and keep one side.
//!
//! The pipeline extracts the audio at the model's sample rate, runs the Python stage in `ml/`
//! (see its README for the contract), caches both stems, then writes the kept one: as a WAV
//! for audio-only inputs, or remuxed with the original video for the rest.

mod cache;
mod child;
mod media;
mod protocol;

use std::{
    fmt,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use tokio::{
    process::Command,
    signal::unix::{SignalKind, signal},
};
use tokio_util::sync::CancellationToken;

use crate::{
    ffprobe::{self, Cancelled, StreamKind},
    tools::sync_check::thousands,
};
use cache::{Cache, Entry, Key};
use media::{AudioStream, Encoding, Output};
use protocol::{Message, Stats};

/// Overrides where `vt` finds the Python project, which defaults to `ml/` in the source tree
/// `vt` was built from.
const ML_PROJECT_ENV: &str = "VT_ML_PROJECT";

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

    /// The `ml/pyproject.toml` extra holding the model's dependencies. Every model runs on the
    /// GPU, so it also needs a `Gpu` extra.
    fn extra(self) -> Option<&'static str> {
        match self {
            Backend::Sam => Some("sam"),
            Backend::Tiger => Some("tiger"),
            Backend::Fake => None,
        }
    }
}

const GPU_ENV: &str = "VT_GPU";

/// The GPU platform, which picks the torch build the Python stage installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gpu {
    Rocm,
    Cuda,
}

impl Gpu {
    /// `VT_GPU` if set, otherwise whichever GPU driver is loaded.
    fn detect() -> Result<Gpu> {
        Gpu::choose(
            std::env::var(GPU_ENV).ok().as_deref(),
            Path::new("/dev/kfd").exists(),
            Path::new("/proc/driver/nvidia/version").exists(),
        )
    }

    fn choose(requested: Option<&str>, amd: bool, nvidia: bool) -> Result<Gpu> {
        match (requested, amd, nvidia) {
            (Some("rocm"), ..) | (None, true, false) => Ok(Gpu::Rocm),
            (Some("cuda"), ..) | (None, false, true) => Ok(Gpu::Cuda),
            (Some(other), ..) => bail!("{GPU_ENV} is {other:?}; set it to rocm or cuda"),
            (None, true, true) => {
                bail!("found AMD and NVIDIA GPU drivers; set {GPU_ENV} to rocm or cuda")
            }
            (None, false, false) => {
                bail!("dialogue-separation needs a GPU, but found no AMD (ROCm) or NVIDIA driver")
            }
        }
    }

    /// The `ml/pyproject.toml` extra holding this platform's torch build.
    fn extra(self) -> &'static str {
        match self {
            Gpu::Rocm => "rocm",
            Gpu::Cuda => "cuda",
        }
    }
}

const SIGINT: u8 = 2;
const SIGTERM: u8 = 15;

/// Exits 0 on success, or 128 plus the signal number when SIGINT or SIGTERM cancels the run.
/// Errors exit 2 from `main`.
pub async fn cli(args: &DialogueSeparationArgs) -> Result<ExitCode> {
    let mut interrupt = signal(SignalKind::interrupt()).context("failed to handle SIGINT")?;
    let mut terminate = signal(SignalKind::terminate()).context("failed to handle SIGTERM")?;
    let show_progress = std::io::stderr().is_terminal();
    let cancel = CancellationToken::new();

    let run = run(&args.file, args.keep, args.backend, &cancel, |progress| {
        if show_progress {
            eprint!("\r\x1b[2K{progress}");
            let _ = std::io::stderr().flush();
        }
    });
    tokio::pin!(run);

    // After a signal, keep polling the run so the stage can clean up before `vt` exits.
    let mut signalled = None;
    let result = loop {
        tokio::select! {
            result = &mut run => break result,
            _ = interrupt.recv(), if signalled.is_none() => signalled = Some(SIGINT),
            _ = terminate.recv(), if signalled.is_none() => signalled = Some(SIGTERM),
        }
        cancel.cancel();
    };
    if show_progress {
        eprint!("\r\x1b[2K");
    }

    if let Some(signal) = signalled
        && result.as_ref().is_err_and(|e| e.is::<Cancelled>())
    {
        eprintln!("Cancelled.");
        return Ok(ExitCode::from(128 + signal));
    }
    for line in result?.lines() {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
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
        if let Some(mb) = peak_vram_mb {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            lines.push(format!("Peak VRAM: {} MB.", thousands(mb.round() as u64)));
        }
        lines
    }
}

/// Separates the first audio stream of `path` and writes the kept side next to it.
pub async fn run(
    path: &Path,
    keep: Keep,
    backend: Backend,
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

    // Decided before separating, which can take an hour, so a problem here fails fast.
    let output = if has_video {
        Output::Remux {
            path: output_path(path, keep, true),
            audio: AudioStream {
                index: audio.index,
                sample_rate,
                start: audio.start.unwrap_or_default(),
            },
            encoding: Encoding::for_stream(path, audio).await?,
        }
    } else {
        Output::Wav {
            path: output_path(path, keep, false),
            sample_rate,
        }
    };

    let cache = Cache::new()?;
    let key = Key::of(path, backend)?;
    let (entry, cached) = match cache.get(&key) {
        Some(entry) => (entry, true),
        None => {
            let entry =
                separate(path, audio, backend, &cache, &key, cancel, &mut on_progress).await?;
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
        cached,
    })
}

async fn separate(
    path: &Path,
    audio: &ffprobe::Stream,
    backend: Backend,
    cache: &Cache,
    key: &Key,
    cancel: &CancellationToken,
    on_progress: &mut impl FnMut(Progress),
) -> Result<Entry> {
    // Dropping `pending` on any early return, including cancellation, deletes the partial entry.
    let pending = cache.begin(key)?;
    let input = pending.dir().join("input.wav");
    // Built first, so a missing GPU fails before extraction.
    let command = stage_command(backend, &input, pending.dir())?;

    on_progress(Progress::Extracting);
    cancel
        .run_until_cancelled(media::extract(path, audio, backend.sample_rate(), &input))
        .await
        .ok_or(Cancelled)??;

    on_progress(Progress::Starting);
    let done = child::run(
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

    let expected = (
        pending.dir().join(cache::VOICE),
        pending.dir().join(cache::AMBIENCE),
    );
    if (done.voice.as_path(), done.ambience.as_path()) != (&expected.0, &expected.1) {
        bail!(
            "dialogue-separation wrote {} and {}, not the expected {} and {}",
            done.voice.display(),
            done.ambience.display(),
            expected.0.display(),
            expected.1.display()
        );
    }
    pending.commit(&done.stats)
}

/// `uv run` syncs the backend's extra before starting, so the first run of a backend installs it.
fn stage_command(backend: Backend, input: &Path, output_dir: &Path) -> Result<Command> {
    let project = std::env::var_os(ML_PROJECT_ENV).map_or_else(
        || PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/ml")),
        PathBuf::from,
    );
    if !project.join("pyproject.toml").is_file() {
        bail!(
            "no Python project at {}; set {ML_PROJECT_ENV} to vt's ml/ directory",
            project.display()
        );
    }
    let mut command = Command::new("uv");
    command.args(["run", "--quiet", "--project"]).arg(project);
    if let Some(extra) = backend.extra() {
        command.args(["--extra", extra, "--extra", Gpu::detect()?.extra()]);
    }
    command
        .arg("dialogue-separation")
        .arg("--input")
        .arg(input)
        .arg("--output-dir")
        .arg(output_dir)
        .args(["--backend", backend.name(), "--device", backend.device()]);
    Ok(command)
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
    fn picks_the_gpu_whose_driver_is_loaded() {
        assert_eq!(Gpu::choose(None, true, false).unwrap(), Gpu::Rocm);
        assert_eq!(Gpu::choose(None, false, true).unwrap(), Gpu::Cuda);
    }

    #[test]
    fn lets_vt_gpu_override_detection() {
        assert_eq!(Gpu::choose(Some("cuda"), true, true).unwrap(), Gpu::Cuda);
        assert_eq!(Gpu::choose(Some("rocm"), false, false).unwrap(), Gpu::Rocm);
    }

    #[test]
    fn asks_which_gpu_when_both_drivers_are_loaded() {
        assert_eq!(
            Gpu::choose(None, true, true).unwrap_err().to_string(),
            "found AMD and NVIDIA GPU drivers; set VT_GPU to rocm or cuda"
        );
    }

    #[test]
    fn fails_without_a_gpu() {
        assert_eq!(
            Gpu::choose(None, false, false).unwrap_err().to_string(),
            "dialogue-separation needs a GPU, but found no AMD (ROCm) or NVIDIA driver"
        );
    }

    #[test]
    fn rejects_an_unknown_vt_gpu() {
        assert_eq!(
            Gpu::choose(Some("metal"), true, false)
                .unwrap_err()
                .to_string(),
            r#"VT_GPU is "metal"; set it to rocm or cuda"#
        );
    }

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
            ]
        );
    }
}
