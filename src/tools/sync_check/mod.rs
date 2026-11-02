//! `vt sync-check`: diagnose audio/video sync problems, and fix them on request.

pub mod diagnose;
mod fix;

use std::{
    fmt,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Args;
use tokio_util::sync::CancellationToken;

use crate::{
    display,
    ffmpeg::{self, AudioEncoding},
    ffprobe::{self, Cancelled, Ratio, StreamKind},
    tools::{self, Tool, cli::Printed},
};
use diagnose::{AudioTrack, Finding, Nanos, Packet, Probe, Span, Track, VideoTrack};
use fix::{AudioPlan, Fixes, Plan, VideoEncoder, VideoFix, VideoPlan};

/// Estimates audio packets per second for the progress bar when the container doesn't count
/// them. This is AAC's frame size.
const ESTIMATED_SAMPLES_PER_AUDIO_PACKET: u32 = 1024;

/// Packets scanned between progress reports.
const PROGRESS_EVERY: u64 = 4096;

#[derive(Debug, Args)]
pub struct SyncCheckArgs {
    /// Video files to check
    #[arg(required = true)]
    files: Vec<PathBuf>,

    /// Fix the issues and write each file's fixed copy next to it, such as `talk.synced.mp4`
    #[arg(long)]
    fix: bool,

    /// Replace existing fixed copies
    #[arg(long, requires = "fix")]
    force: bool,
}

/// Flags a file when it has findings.
pub async fn cli(args: &SyncCheckArgs) -> Result<ExitCode> {
    let mode = if args.fix {
        Mode::Fix { force: args.force }
    } else {
        Mode::Check
    };
    let cancel = CancellationToken::new();
    tools::cli::run(Tool::SyncCheck, &args.files, &cancel, |file| {
        let cancel = &cancel;
        async move {
            let outcome = run(file, mode, cancel, |progress| {
                if let Mode::Fix { .. } = mode {
                    tools::cli::show_progress(file, &progress);
                }
            })
            .await
            .map_err(tools::suggest_force)?;
            Ok(printed(file, &outcome))
        }
    })
    .await
}

fn printed(file: &Path, outcome: &Outcome) -> Printed {
    let report = &outcome.report;
    let mut stdout: Vec<String> = report.header().into_iter().collect();
    stdout.push(String::new());
    if report.findings.is_empty() {
        stdout.push(NO_ISSUES.to_string());
    }
    stdout.extend(report.findings.iter().map(ToString::to_string));
    match &outcome.fixed {
        None if report.fixable() > 0 => {
            stdout.push(String::new());
            stdout.push(format!(
                "Run `vt sync-check --fix {}` to write a fixed copy.",
                display::path(file)
            ));
        }
        Some(fixed) if !report.findings.is_empty() => {
            stdout.push(String::new());
            stdout.push(fixed.summary());
            if let Fixed::Wrote { remaining, .. } = fixed
                && !remaining.is_empty()
            {
                stdout.extend(remaining.iter().map(ToString::to_string));
            }
        }
        None | Some(_) => {}
    }
    Printed {
        stdout,
        stderr: vec![],
        flagged: !report.findings.is_empty(),
    }
}

pub const NO_ISSUES: &str = "No sync issues found.";

/// Whether a run also fixes what it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Check,

    /// Writes a fixed copy, replacing an existing one only if `force` is set.
    Fix {
        force: bool,
    },
}

#[derive(Debug)]
pub struct Outcome {
    pub report: Report,

    /// `None` when only checking.
    pub fixed: Option<Fixed>,
}

#[derive(Debug)]
pub enum Fixed {
    /// No finding has an automatic fix.
    Nothing,

    Wrote {
        output: PathBuf,

        /// The findings the copy fixes.
        fixed: usize,

        /// The findings with an automatic fix that a re-check of the copy still found.
        remaining: Vec<Finding>,
    },
}

impl Fixed {
    /// What the fix did, in one line. A re-check that still found issues lists them below it.
    pub fn summary(&self) -> String {
        match self {
            Fixed::Nothing => "Nothing to fix automatically.".into(),
            Fixed::Wrote {
                output,
                fixed,
                remaining,
            } => {
                let output = display::name(output);
                let issues = if *fixed == 1 { "issue" } else { "issues" };
                if remaining.is_empty() {
                    format!("Fixed {fixed} {issues} in {output}.")
                } else {
                    format!("Wrote {output}, but re-checking it still found:")
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Scanning(Scan),

    Writing {
        written: Duration,

        /// `None` when the duration is unknown.
        total: Option<Duration>,
    },

    Rechecking(Scan),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scan {
    pub packets: u64,

    /// Estimated from the duration and stream rates. `None` when the duration is unknown.
    pub expected: Option<u64>,
}

impl Progress {
    /// The fraction done. Totals are estimates, so it holds below 100% until the run ends.
    #[allow(clippy::cast_precision_loss)]
    pub fn ratio(&self) -> Option<f64> {
        let ratio = match *self {
            Progress::Scanning(scan) | Progress::Rechecking(scan) => {
                let expected = scan.expected.filter(|&n| n > 0)?;
                scan.packets as f64 / expected as f64
            }
            Progress::Writing { written, total } => {
                let total = total.filter(|t| !t.is_zero())?;
                written.as_secs_f64() / total.as_secs_f64()
            }
        };
        Some(ratio.clamp(0.0, 0.99))
    }
}

impl fmt::Display for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ratio = self.ratio();
        let what = match *self {
            Progress::Scanning(Scan { packets, expected }) => match expected {
                Some(expected) if ratio.is_some() => {
                    format!("{} of ~{} packets", thousands(packets), thousands(expected))
                }
                _ => format!("{} packets", thousands(packets)),
            },
            Progress::Writing { .. } => "writing the fixed copy".into(),
            Progress::Rechecking(_) => "re-checking the fixed copy".into(),
        };
        match ratio {
            Some(ratio) => write!(f, "{:.0}% · {what}", ratio * 100.0),
            None => f.write_str(&what),
        }
    }
}

/// Scans every packet in `path` and diagnoses it. When fixing, also writes a fixed copy next to
/// it and re-checks the copy.
pub async fn run(
    path: &Path,
    mode: Mode,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(Progress),
) -> Result<Outcome> {
    let output = match mode {
        Mode::Check => None,
        Mode::Fix { force } => {
            let output = output_path(path);
            tools::check_output(&output, force)?;
            Some(output)
        }
    };
    let info = ffprobe::streams(path).await?;
    let probe = scan(path, &info, cancel, |s| on_progress(Progress::Scanning(s))).await?;
    let report = Report::new(&probe);
    let fixed = match output {
        None => None,
        Some(output) => {
            let fixes = Fixes::new(&report.findings);
            Some(if fixes.is_empty() {
                Fixed::Nothing
            } else {
                let fixed = fix::fixable(&report.findings).count();
                let plan = plan(path, &info, &probe, &fixes).await?;
                let total = info.duration;
                let remaining =
                    write(path, &plan, &output, total, cancel, &mut on_progress).await?;
                Fixed::Wrote {
                    output,
                    fixed,
                    remaining,
                }
            })
        }
    };
    Ok(Outcome { report, fixed })
}

/// `talk.mp4` becomes `talk.synced.mp4`.
pub fn output_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let ext = input
        .extension()
        .map_or_else(|| "mkv".into(), |e| e.to_string_lossy());
    input.with_file_name(format!("{stem}.synced.{ext}"))
}

/// Decides how to write each stream, before any slow work.
async fn plan(path: &Path, info: &ffprobe::Streams, probe: &Probe, fixes: &Fixes) -> Result<Plan> {
    let video = &probe.video;
    let video_plan = match fixes.video {
        None => VideoPlan::Copy,
        Some(VideoFix::Retime) => VideoPlan::Retime {
            time_base: video.track.time_base,
            frame_rate: video.frame_rate,
        },
        Some(VideoFix::Reencode) => VideoPlan::Reencode {
            encoder: VideoEncoder::for_codec(&video.track.codec)?,
            frame_rate: video.frame_rate,
        },
    };
    let mut audio_plans = Vec::new();
    for (position, audio) in probe.audio.iter().enumerate() {
        let index = audio.track.index;
        if !fixes.audio.contains(&index) {
            continue;
        }
        let stream = info
            .streams
            .iter()
            .find(|s| s.index == index)
            .with_context(|| format!("audio #{index} not found"))?;
        // A resynced stream has packets, so it has a span. Without video, it keeps its start.
        let video_start = video
            .span()
            .or_else(|| audio.span())
            .map_or_else(Nanos::default, |span| span.start);
        audio_plans.push(AudioPlan {
            position,
            sample_rate: audio.sample_rate,
            video_start,
            encoding: AudioEncoding::for_stream(path, stream).await?,
        });
    }
    Ok(Plan {
        video: video_plan,
        audio: audio_plans,
    })
}

/// Writes `output` as `plan` says, then re-checks it and returns the findings with an automatic
/// fix that it still has. `total` is the input's duration.
async fn write(
    path: &Path,
    plan: &Plan,
    output: &Path,
    total: Option<Duration>,
    cancel: &CancellationToken,
    on_progress: &mut impl FnMut(Progress),
) -> Result<Vec<Finding>> {
    let written = ffmpeg::write(plan.args(path), output, |written| {
        on_progress(Progress::Writing { written, total });
    });
    cancel
        .run_until_cancelled(written)
        .await
        .ok_or(Cancelled)??;

    let info = ffprobe::streams(output).await?;
    let probe = scan(output, &info, cancel, |s| {
        on_progress(Progress::Rechecking(s))
    })
    .await?;
    Ok(fix::fixable(&diagnose::diagnose(&probe)).cloned().collect())
}

/// Reads every packet of the first video stream and each audio stream in `path`.
async fn scan(
    path: &Path,
    info: &ffprobe::Streams,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(Scan),
) -> Result<Probe> {
    let video = info
        .streams
        .iter()
        .find(|s| matches!(s.kind, StreamKind::Video { .. }))
        .context("no video stream")?;
    let audio: Vec<_> = info
        .streams
        .iter()
        .filter(|s| matches!(s.kind, StreamKind::Audio { .. }))
        .collect();
    let tracked: Vec<_> = std::iter::once(video)
        .chain(audio.iter().copied())
        .collect();

    let expected = info
        .duration
        .map(|duration| tracked.iter().map(|s| estimate_packets(s, duration)).sum());
    let mut collected = vec![Vec::new(); tracked.len()];
    let mut packets: u64 = 0;
    ffprobe::packets(path, cancel, |packet| {
        let Some(i) = tracked.iter().position(|s| s.index == packet.stream) else {
            return;
        };
        packets += 1;
        if packets.is_multiple_of(PROGRESS_EVERY) {
            on_progress(Scan { packets, expected });
        }
        if !packet.discard {
            collected[i].push(Packet {
                pts: packet.pts,
                duration: packet.duration,
            });
        }
    })
    .await?;

    // `tracked` lists the video stream first, then the audio streams in order.
    let mut tracks = tracked.iter().zip(collected).map(|(stream, packets)| {
        let track = Track::new(
            stream.index,
            stream.codec.clone(),
            stream.time_base,
            packets,
        );
        (*stream, track)
    });
    let (_, video_track) = tracks.next().context("video stream not tracked")?;
    let StreamKind::Video { frame_rate } = video.kind else {
        unreachable!("video was found by kind");
    };
    Ok(Probe {
        video: VideoTrack {
            track: video_track,
            frame_rate: frame_rate
                .with_context(|| format!("video #{} has no frame rate", video.index))?,
        },
        audio: tracks
            .map(|(stream, track)| {
                let StreamKind::Audio { sample_rate, .. } = stream.kind else {
                    unreachable!("audio was found by kind");
                };
                AudioTrack { track, sample_rate }
            })
            .collect(),
    })
}

/// Estimates a stream's packet count in a file lasting `duration`: exact when the container
/// indexes packets, otherwise from the stream's rate. Progress-bar precision is all this needs,
/// so the fallback uses floats.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn estimate_packets(stream: &ffprobe::Stream, duration: Duration) -> u64 {
    let per_sec = match stream.kind {
        StreamKind::Video {
            frame_rate: Some(rate),
        } => rate.num() as f64 / rate.den() as f64,
        StreamKind::Audio { sample_rate, .. } => {
            f64::from(sample_rate) / f64::from(ESTIMATED_SAMPLES_PER_AUDIO_PACKET)
        }
        _ => 0.0,
    };
    stream
        .packet_count
        .unwrap_or((per_sec * duration.as_secs_f64()) as u64)
}

#[derive(Debug)]
pub struct Report {
    video: VideoSummary,
    audio: Vec<AudioSummary>,
    pub findings: Vec<Finding>,
}

#[derive(Debug)]
struct VideoSummary {
    index: usize,
    codec: String,
    frame_rate: Ratio,
    packets: usize,
    span: Option<Span>,
}

#[derive(Debug)]
struct AudioSummary {
    index: usize,
    codec: String,
    sample_rate: u32,
    packets: usize,
    span: Option<Span>,
}

impl Report {
    pub fn new(probe: &Probe) -> Self {
        let video = &probe.video;
        Report {
            video: VideoSummary {
                index: video.track.index,
                codec: video.track.codec.clone(),
                frame_rate: video.frame_rate,
                packets: video.track.packet_count(),
                span: video.span(),
            },
            audio: probe
                .audio
                .iter()
                .map(|audio| AudioSummary {
                    index: audio.track.index,
                    codec: audio.track.codec.clone(),
                    sample_rate: audio.sample_rate,
                    packets: audio.track.packet_count(),
                    span: audio.span(),
                })
                .collect(),
            findings: diagnose::diagnose(probe),
        }
    }

    /// How many findings have an automatic fix.
    pub fn fixable(&self) -> usize {
        fix::fixable(&self.findings).count()
    }

    /// One line per stream.
    pub fn header(&self) -> Vec<String> {
        let video = &self.video;
        let mut lines = vec![format!(
            "Video #{}: {}, {} fps, {} frames{}",
            video.index,
            video.codec,
            fps(video.frame_rate),
            thousands(video.packets),
            span(video.span)
        )];
        lines.extend(self.audio.iter().map(|audio| {
            format!(
                "Audio #{}: {}, {} Hz, {} packets{}",
                audio.index,
                audio.codec,
                audio.sample_rate,
                thousands(audio.packets),
                span(audio.span)
            )
        }));
        if self.audio.is_empty() {
            lines.push("No audio streams.".into());
        }
        lines
    }
}

fn fps(rate: Ratio) -> String {
    if rate.den() == 1 {
        rate.num().to_string()
    } else {
        format!("{:.3}", rate.num() as f64 / rate.den() as f64)
    }
}

fn span(span: Option<Span>) -> String {
    span.map_or_else(String::new, |s| {
        format!(", {} to {}", timestamp(s.start), timestamp(s.end))
    })
}

/// `[-][H:]M:SS.mmm`, rounded to the millisecond.
fn timestamp(time: Nanos) -> String {
    let millis = time.round_millis();
    let sign = if millis < 0 { "-" } else { "" };
    let millis = millis.unsigned_abs();
    let (secs, millis) = (millis / 1000, millis % 1000);
    let (mins, secs) = (secs / 60, secs % 60);
    if mins >= 60 {
        format!("{sign}{}:{:02}:{secs:02}.{millis:03}", mins / 60, mins % 60)
    } else {
        format!("{sign}{mins}:{secs:02}.{millis:03}")
    }
}

/// Groups digits in threes, e.g. `1,234,567`.
pub fn thousands(n: impl ToString) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn formats_timestamps() {
        let ms = |millis| timestamp(Nanos::from_millis(millis));

        assert_eq!(ms(0), "0:00.000");
        assert_eq!(ms(-21), "-0:00.021");
        assert_eq!(ms(718_208), "11:58.208");
        assert_eq!(ms(4_502_005), "1:15:02.005");
    }

    #[test]
    fn formats_frame_rates() {
        assert_eq!(fps(Ratio::new(60, 1).unwrap()), "60");
        assert_eq!(fps(Ratio::new(30_000, 1001).unwrap()), "29.970");
    }
}
