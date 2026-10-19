//! `vt sync-check`: diagnose audio/video sync problems and recommend a fix.

pub mod diagnose;

use std::{path::Path, path::PathBuf, process::ExitCode, time::Duration};

use anyhow::{Context, Result};
use clap::Args;
use tokio_util::sync::CancellationToken;

use crate::ffprobe::{self, Ratio, StreamKind};
use diagnose::{AudioTrack, Finding, Nanos, Packet, Probe, Span, Track, VideoTrack};

/// Estimates audio packets per second for the progress bar when the container doesn't count
/// them. This is AAC's frame size.
const ESTIMATED_SAMPLES_PER_AUDIO_PACKET: u32 = 1024;

/// Packets scanned between progress reports.
const PROGRESS_EVERY: u64 = 4096;

#[derive(Debug, Args)]
pub struct SyncCheckArgs {
    /// Video file to check
    file: PathBuf,
}

/// Exits 0 when clean and 1 when there are findings. Errors exit 2 from `main`.
pub async fn cli(args: &SyncCheckArgs) -> Result<ExitCode> {
    let report = run(&args.file, &CancellationToken::new(), |_| {}).await?;
    for line in report.header() {
        println!("{line}");
    }
    println!();
    if report.findings.is_empty() {
        println!("{NO_ISSUES}");
    }
    for finding in &report.findings {
        println!("{finding}");
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

/// Scans every packet in `path` and diagnoses it.
pub async fn run(
    path: &Path,
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
            on_progress(Progress { packets, expected });
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
    Ok(Report::new(&probe))
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
        StreamKind::Audio { sample_rate } => {
            f64::from(sample_rate) / f64::from(ESTIMATED_SAMPLES_PER_AUDIO_PACKET)
        }
        _ => 0.0,
    };
    stream
        .packet_count
        .unwrap_or((per_sec * duration.as_secs_f64()) as u64)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub video: VideoSummary,
    pub audio: Vec<AudioSummary>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoSummary {
    pub index: usize,
    pub codec: String,
    pub frame_rate: Ratio,
    pub packets: usize,
    pub span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioSummary {
    pub index: usize,
    pub codec: String,
    pub sample_rate: u32,
    pub packets: usize,
    pub span: Option<Span>,
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
