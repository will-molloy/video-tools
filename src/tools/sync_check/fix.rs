//! Turns a diagnosis into one ffmpeg run that writes a fixed copy.
//!
//! Each stream gets the cheapest fix its findings allow. Video is copied, retimed without
//! re-encoding, or re-encoded at a constant rate. Audio is copied, or re-encoded with silence
//! padded in and early audio trimmed to match its timestamps. Everything else is copied.

use std::{collections::BTreeSet, ffi::OsString, path::Path};

use anyhow::{Result, bail};

use super::diagnose::{Action, Finding, Nanos};
use crate::{ffmpeg::AudioEncoding, ffprobe::Ratio};

/// What fixing a finding does to its stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fix {
    Video(VideoFix),

    /// Re-encode the audio with silence in its gaps and its start moved to the video's.
    ResyncAudio,
}

/// Ordered so that the fix that also covers the other comes last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VideoFix {
    Retime,
    Reencode,
}

/// The fix for `action`, or `None` when it has no automatic fix.
fn fix(action: Action) -> Option<Fix> {
    match action {
        Action::RetimeToCfr => Some(Fix::Video(VideoFix::Retime)),
        Action::ReencodeCfr => Some(Fix::Video(VideoFix::Reencode)),
        Action::PadAudioStart | Action::TrimAudioStart | Action::FillAudioGaps => {
            Some(Fix::ResyncAudio)
        }
        // Timestamps can't tell clock drift from a truncated tail, and stretching a truncated
        // tail would put correct audio out of sync.
        Action::StretchAudio | Action::None | Action::Unsupported => None,
    }
}

/// The findings with an automatic fix.
pub fn fixable(findings: &[Finding]) -> impl Iterator<Item = &Finding> {
    findings.iter().filter(|f| fix(f.action).is_some())
}

/// The fixes a diagnosis calls for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Fixes {
    pub video: Option<VideoFix>,

    /// The audio streams to resync, by stream index.
    pub audio: BTreeSet<usize>,
}

impl Fixes {
    pub fn new(findings: &[Finding]) -> Fixes {
        let mut fixes = Fixes::default();
        for finding in findings {
            match fix(finding.action) {
                Some(Fix::Video(video)) => fixes.video = fixes.video.max(Some(video)),
                Some(Fix::ResyncAudio) => {
                    fixes.audio.insert(finding.stream);
                }
                None => {}
            }
        }
        fixes
    }

    pub fn is_empty(&self) -> bool {
        self.video.is_none() && self.audio.is_empty()
    }
}

/// One ffmpeg run that writes the fixed copy.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub video: VideoPlan,

    /// The audio streams to resync. The rest are copied.
    pub audio: Vec<AudioPlan>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VideoPlan {
    Copy,

    /// Snaps each frame to the nearest multiple of the frame duration, without re-encoding.
    Retime {
        time_base: Ratio,
        frame_rate: Ratio,
    },

    /// Re-encodes at a constant rate, duplicating or dropping frames to keep each in place.
    Reencode {
        encoder: VideoEncoder,
        frame_rate: Ratio,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioPlan {
    /// The stream's position among the audio streams, which is its position in the output.
    pub position: usize,
    pub sample_rate: u32,

    /// When the first video frame plays, which the first audio sample moves to.
    pub video_start: Nanos,
    pub encoding: AudioEncoding,
}

/// The encoder that re-encodes video in its own codec, at a quality that hides the generation
/// loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoder {
    X264,
    X265,
}

impl VideoEncoder {
    pub fn for_codec(codec: &str) -> Result<VideoEncoder> {
        match codec {
            "h264" => Ok(VideoEncoder::X264),
            "hevc" => Ok(VideoEncoder::X265),
            codec => bail!("can't re-encode {codec} video, only h264 and hevc"),
        }
    }

    /// x265's CRF runs about 5 higher than x264's for the same quality.
    fn args(self) -> [&'static str; 3] {
        match self {
            VideoEncoder::X264 => ["libx264", "-crf", "18"],
            VideoEncoder::X265 => ["libx265", "-crf", "23"],
        }
    }
}

/// Audio timestamps further than this from where the samples so far put them are corrected
/// with silence or a cut. Smaller ones are rounding, such as Matroska's to the millisecond.
const RESYNC_THRESHOLD_SECS: &str = "0.001";

impl Plan {
    /// The ffmpeg arguments that read `input` and fix it, up to the output path.
    pub fn args(&self, input: &Path) -> Vec<OsString> {
        // `-copyts` keeps the source timestamps, which the fixes are measured against.
        let mut args: Vec<OsString> = vec!["-copyts".into(), "-i".into(), input.into()];

        // `V` skips cover art, which ffprobe models as a one-frame video stream.
        for map in ["0:V", "0:a", "0:s?", "0:t?"] {
            args.extend(["-map".into(), map.into()]);
        }
        args.extend(["-c".into(), "copy".into()]);
        match &self.video {
            VideoPlan::Copy => {}
            VideoPlan::Retime {
                time_base,
                frame_rate,
            } => args.extend(["-bsf:v:0".into(), setts(*time_base, *frame_rate).into()]),
            VideoPlan::Reencode {
                encoder,
                frame_rate,
            } => {
                let fps = format!("fps={}/{}", frame_rate.num(), frame_rate.den());
                args.extend(["-filter:v:0".into(), fps.into(), "-c:v:0".into()]);
                args.extend(encoder.args().map(OsString::from));
            }
        }
        for audio in &self.audio {
            let spec = format!("a:{}", audio.position);
            let first_pts = samples(audio.video_start, audio.sample_rate);
            let resync = format!(
                "aresample=async=1:min_hard_comp={RESYNC_THRESHOLD_SECS}:first_pts={first_pts}"
            );
            args.extend([format!("-filter:{spec}").into(), resync.into()]);
            args.extend(audio.encoding.args(&spec));
        }
        args
    }
}

/// A `setts` filter that rounds each timestamp to the nearest whole frame after the first.
fn setts(time_base: Ratio, frame_rate: Ratio) -> String {
    // The frame duration in ticks is `num / den`.
    let num = time_base.den() * frame_rate.den();
    let den = time_base.num() * frame_rate.num();
    let snap = |option: &str, ts: &str| {
        format!("{option}=round(round(({ts}-START{ts})*{den}/{num})*{num}/{den})+START{ts}")
    };
    format!("setts={}:{}", snap("pts", "PTS"), snap("dts", "DTS"))
}

/// `time` in samples at `sample_rate`, rounded half up.
fn samples(time: Nanos, sample_rate: u32) -> i64 {
    let scaled = i128::from(time.as_nanos()) * i128::from(sample_rate);
    let samples = (2 * scaled + 1_000_000_000).div_euclid(2_000_000_000);
    i64::try_from(samples).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sync_check::diagnose::{FindingKind, Severity};

    fn finding(stream: usize, action: Action) -> Finding {
        Finding {
            stream,
            severity: Severity::Major,
            action,
            kind: FindingKind::AudioStartOffset {
                offset: Nanos::from_millis(500),
            },
        }
    }

    #[test]
    fn fixes_each_stream_once_preferring_a_reencode() {
        let findings = [
            finding(0, Action::ReencodeCfr),
            finding(0, Action::RetimeToCfr),
            finding(1, Action::PadAudioStart),
            finding(1, Action::FillAudioGaps),
            finding(2, Action::None),
            finding(3, Action::TrimAudioStart),
        ];

        assert_eq!(
            Fixes::new(&findings),
            Fixes {
                video: Some(VideoFix::Reencode),
                audio: BTreeSet::from([1, 3]),
            }
        );
    }

    #[test]
    fn leaves_a_duration_mismatch_alone() {
        assert!(Fixes::new(&[finding(1, Action::StretchAudio)]).is_empty());
    }

    fn args(plan: &Plan) -> Vec<String> {
        plan.args(Path::new("/v/talk.mkv"))
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    const COPY: [&str; 13] = [
        "-copyts",
        "-i",
        "/v/talk.mkv",
        "-map",
        "0:V",
        "-map",
        "0:a",
        "-map",
        "0:s?",
        "-map",
        "0:t?",
        "-c",
        "copy",
    ];

    fn rate(num: i64, den: i64) -> Ratio {
        Ratio::new(num, den).unwrap()
    }

    #[test]
    fn retimes_matroska_video_to_whole_frames() {
        let plan = Plan {
            video: VideoPlan::Retime {
                time_base: rate(1, 1000),
                frame_rate: rate(60, 1),
            },
            audio: vec![],
        };

        assert_eq!(
            args(&plan)[COPY.len()..],
            [
                "-bsf:v:0",
                "setts=pts=round(round((PTS-STARTPTS)*60/1000)*1000/60)+STARTPTS:\
                 dts=round(round((DTS-STARTDTS)*60/1000)*1000/60)+STARTDTS"
            ]
        );
    }

    #[test]
    fn reencodes_video_and_resyncs_the_second_audio_stream() {
        let plan = Plan {
            video: VideoPlan::Reencode {
                encoder: VideoEncoder::X264,
                frame_rate: rate(30_000, 1001),
            },
            audio: vec![AudioPlan {
                position: 1,
                sample_rate: 48_000,
                video_start: Nanos::from_millis(521),
                encoding: AudioEncoding {
                    encoder: "aac".into(),
                    bit_rate: Some(64_000),
                },
            }],
        };

        let mut expected = COPY.to_vec();
        expected.extend([
            "-filter:v:0",
            "fps=30000/1001",
            "-c:v:0",
            "libx264",
            "-crf",
            "18",
            "-filter:a:1",
            "aresample=async=1:min_hard_comp=0.001:first_pts=25008",
            "-c:a:1",
            "aac",
            "-b:a:1",
            "64000",
        ]);
        assert_eq!(args(&plan), expected);
    }

    #[test]
    fn re_encodes_only_h264_and_hevc() {
        assert_eq!(VideoEncoder::for_codec("hevc").unwrap(), VideoEncoder::X265);
        assert_eq!(
            VideoEncoder::for_codec("vp9").unwrap_err().to_string(),
            "can't re-encode vp9 video, only h264 and hevc"
        );
    }
}
