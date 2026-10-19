//! `vt sync-check`: diagnose audio/video sync problems and recommend a fix.

pub mod diagnose;

use std::{path::Path, path::PathBuf, process::ExitCode, time::Duration};

use anyhow::{Context, Result};
use clap::Args;
use serde::{Serialize, Serializer};
use tokio_util::sync::CancellationToken;

use crate::ffprobe::{self, Ratio, ReadInterval, StreamKind};
use diagnose::{AudioTrack, Finding, Nanos, Packet, Probe, Segment, Span, Track, VideoTrack};

/// The default scan reads this much from the start of the file...
pub const SAMPLE_HEAD: Duration = Duration::from_secs(10 * 60);

/// ...and this much from the end, to find where each stream stops.
pub const SAMPLE_TAIL: Duration = Duration::from_secs(30);

/// Estimates audio packets per second for the progress bar when the container doesn't count
/// them. This is AAC's frame size.
const ESTIMATED_SAMPLES_PER_AUDIO_PACKET: u32 = 1024;

/// Packets scanned between progress reports.
const PROGRESS_EVERY: u64 = 4096;

#[derive(Debug, Args)]
pub struct SyncCheckArgs {
    /// Video file to check
    file: PathBuf,

    /// Scan the whole file instead of its first 10 minutes and last 30 seconds
    #[arg(long)]
    full: bool,

    /// Print the report as JSON
    #[arg(long)]
    json: bool,
}

/// Exits 0 when clean and 1 when there are findings. Errors exit 2 from `main`.
pub async fn cli(args: &SyncCheckArgs) -> Result<ExitCode> {
    let report = run(&args.file, args.full, &CancellationToken::new(), |_| {}).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for line in report.header() {
            println!("{line}");
        }
        if matches!(report.scope, Scope::Sampled { .. }) {
            println!("Pass --full to scan the whole file.");
        }
        println!();
        if report.findings.is_empty() {
            println!("{NO_ISSUES}");
        }
        for finding in &report.findings {
            println!("{finding}");
        }
    }
    Ok(if report.findings.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub const NO_ISSUES: &str = "No sync issues found.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub packets: u64,

    /// Estimated from the duration and stream rates. `None` when the duration is unknown.
    pub expected: Option<u64>,
}

/// Probes `path` and diagnoses it, sampling unless `full` is set.
pub async fn run(
    path: &Path,
    full: bool,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(Progress),
) -> Result<Report> {
    let info = ffprobe::streams(path).await?;
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

    let scope = Scope::new(full, info.duration);
    let intervals = scope.intervals(info.duration);
    let expected = info.duration.map(|duration| {
        intervals
            .iter()
            .map(|&i| estimate_packets(&tracked, duration, interval_length(i, duration)))
            .sum()
    });

    let mut segments = vec![Vec::new(); tracked.len()];
    let mut packets: u64 = 0;
    for &interval in &intervals {
        let mut current = vec![Vec::new(); tracked.len()];
        ffprobe::packets(path, interval, cancel, |packet| {
            let Some(i) = tracked.iter().position(|s| s.index == packet.stream) else {
                return;
            };
            packets += 1;
            if packets.is_multiple_of(PROGRESS_EVERY) {
                on_progress(Progress { packets, expected });
            }
            if !packet.discard {
                current[i].push(Packet {
                    pts: packet.pts,
                    duration: packet.duration,
                });
            }
        })
        .await?;
        for (track, packets) in segments.iter_mut().zip(current) {
            track.push(Segment::new(packets));
        }
    }

    // `tracked` lists the video stream first, then the audio streams in order.
    let mut tracks = tracked
        .iter()
        .zip(segments)
        .map(|(stream, segments)| (*stream, track(stream, segments)));
    let (_, video_track) = tracks.next().context("video stream not tracked")?;
    let StreamKind::Video { frame_rate } = video.kind else {
        unreachable!("video was found by kind");
    };
    let probe = Probe {
        video: VideoTrack {
            track: video_track,
            frame_rate: frame_rate
                .with_context(|| format!("video #{} has no frame rate", video.index))?,
        },
        audio: tracks
            .map(|(stream, track)| {
                let StreamKind::Audio { sample_rate } = stream.kind else {
                    unreachable!("audio was found by kind");
                };
                AudioTrack { track, sample_rate }
            })
            .collect(),
    };
    Ok(Report::new(scope, &probe))
}

fn track(stream: &ffprobe::Stream, segments: Vec<Segment>) -> Track {
    Track {
        index: stream.index,
        codec: stream.codec.clone(),
        time_base: stream.time_base,
        segments,
    }
}

fn interval_length(interval: ReadInterval, duration: Duration) -> Duration {
    match interval {
        ReadInterval::Whole => duration,
        ReadInterval::Head(length) => length.min(duration),
        ReadInterval::From(start) => duration.saturating_sub(start),
    }
}

/// Estimates how many packets the tracked streams have in `length` of a file lasting
/// `duration`. Progress-bar precision is all this needs, so it uses floats.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn estimate_packets(streams: &[&ffprobe::Stream], duration: Duration, length: Duration) -> u64 {
    let per_sec = |stream: &ffprobe::Stream| match (stream.packet_count, &stream.kind) {
        (Some(count), _) if !duration.is_zero() => count as f64 / duration.as_secs_f64(),
        (
            _,
            StreamKind::Video {
                frame_rate: Some(rate),
            },
        ) => rate.num() as f64 / rate.den() as f64,
        (_, StreamKind::Audio { sample_rate }) => {
            f64::from(*sample_rate) / f64::from(ESTIMATED_SAMPLES_PER_AUDIO_PACKET)
        }
        _ => 0.0,
    };
    streams
        .iter()
        .map(|stream| (per_sec(stream) * length.as_secs_f64()) as u64)
        .sum()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Scope {
    Full,
    Sampled {
        #[serde(serialize_with = "seconds")]
        head: Duration,
        #[serde(serialize_with = "seconds")]
        tail: Duration,
    },
}

impl Scope {
    /// Samples unless `full` is set, the duration is unknown, or the samples would cover the
    /// whole file anyway.
    fn new(full: bool, duration: Option<Duration>) -> Self {
        match duration {
            Some(duration) if !full && duration > SAMPLE_HEAD + SAMPLE_TAIL => Scope::Sampled {
                head: SAMPLE_HEAD,
                tail: SAMPLE_TAIL,
            },
            _ => Scope::Full,
        }
    }

    fn intervals(self, duration: Option<Duration>) -> Vec<ReadInterval> {
        match (self, duration) {
            (Scope::Sampled { head, tail }, Some(duration)) => vec![
                ReadInterval::Head(head),
                ReadInterval::From(duration.saturating_sub(tail)),
            ],
            _ => vec![ReadInterval::Whole],
        }
    }
}

fn seconds<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_f64(duration.as_secs_f64())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub scope: Scope,
    pub video: VideoSummary,
    pub audio: Vec<AudioSummary>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoSummary {
    pub index: usize,
    pub codec: String,
    #[serde(serialize_with = "ratio")]
    pub frame_rate: Ratio,
    pub packets: usize,
    pub span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioSummary {
    pub index: usize,
    pub codec: String,
    pub sample_rate: u32,
    pub packets: usize,
    pub span: Option<Span>,
}

fn ratio<S: Serializer>(ratio: &Ratio, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(ratio)
}

impl Report {
    pub fn new(scope: Scope, probe: &Probe) -> Self {
        let video = &probe.video;
        Report {
            scope,
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

    /// The scan scope and one line per stream.
    pub fn header(&self) -> Vec<String> {
        let scope = match self.scope {
            Scope::Full => "Scanned the whole file.".to_string(),
            Scope::Sampled { head, tail } => format!(
                "Scanned the first {} and last {} only.",
                clock(head),
                clock(tail)
            ),
        };
        let video = &self.video;
        let mut lines = vec![
            scope,
            format!(
                "Video #{}: {}, {} fps, {} frames{}",
                video.index,
                video.codec,
                fps(video.frame_rate),
                thousands(video.packets),
                span(video.span)
            ),
        ];
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

/// `M:SS` for whole durations such as the sample lengths.
fn clock(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
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
    fn samples_files_longer_than_head_and_tail() {
        let longer = SAMPLE_HEAD + SAMPLE_TAIL + Duration::from_millis(1);

        assert_eq!(
            Scope::new(false, Some(longer)),
            Scope::Sampled {
                head: SAMPLE_HEAD,
                tail: SAMPLE_TAIL,
            }
        );
        assert_eq!(
            Scope::new(false, Some(longer)).intervals(Some(longer)),
            vec![
                ReadInterval::Head(SAMPLE_HEAD),
                ReadInterval::From(SAMPLE_HEAD + Duration::from_millis(1)),
            ]
        );
    }

    #[test]
    fn scans_whole_file_when_full_short_or_of_unknown_length() {
        let long = Duration::from_secs(3600);

        assert_eq!(Scope::new(true, Some(long)), Scope::Full);
        assert_eq!(
            Scope::new(false, Some(SAMPLE_HEAD + SAMPLE_TAIL)),
            Scope::Full
        );
        assert_eq!(Scope::new(false, None), Scope::Full);
        assert_eq!(Scope::Full.intervals(Some(long)), vec![ReadInterval::Whole]);
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
