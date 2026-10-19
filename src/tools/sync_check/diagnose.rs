//! Sync diagnosis as a pure function from scanned packet timestamps to findings.
//!
//! Timing math stays in integer ticks of each stream's time base, so constant-rate intervals
//! compare exactly. Values become nanoseconds only to compare streams with different time bases
//! and for reporting.

use std::fmt;

use serde::{Serialize, Serializer};

use crate::ffprobe::Ratio;

/// Video intervals within this many ticks of the nominal frame duration count as regular.
/// Matroska's 1 ms time base stores 60 fps as a mix of 16 and 17 ms intervals.
pub const CFR_TOLERANCE_TICKS: i64 = 1;

/// A video interval at least this many nominal frame durations long has frames missing.
pub const DROP_FACTOR: Factor = Factor::new(3, 2);

/// An audio interval longer than this many packet durations has a gap.
pub const GAP_FACTOR: Factor = Factor::new(3, 2);

/// Timing errors shorter than this many video frames are ignored, because video can't place
/// content more finely. Retiming to CFR moves each frame by its drift, so it is safe only while
/// drift stays below this floor too.
pub const VIDEO_NOISE_FLOOR_FRAMES: i64 = 1;

/// Audio timing errors shorter than this many audio packets are ignored too. Encoders frame
/// audio in whole packets, leaving up to one packet of priming at the start (kept as real audio
/// in Matroska) and one of padding at the end. Two AAC packets (42.7 ms) stay below
/// [`PERCEPTIBLE`].
pub const AUDIO_NOISE_FLOOR_FRAMES: i64 = 2;

/// Desync at or above this is perceptible. ITU-R BT.1359 puts the detection threshold at 45 ms
/// for audio leading video, the stricter direction.
pub const PERCEPTIBLE: Nanos = Nanos::from_millis(45);

/// A positive multiplier applied to a frame duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Factor {
    num: i128,
    den: i128,
}

impl Factor {
    const fn new(num: i128, den: i128) -> Self {
        Factor { num, den }
    }

    /// Whether `value` is at least this factor times `unit`.
    fn reached_by(self, value: i128, unit: i128) -> bool {
        value * self.den >= unit * self.num
    }
}

/// What the probe found, with each stream's packets grouped by scanned interval.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub video: VideoTrack,
    pub audio: Vec<AudioTrack>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoTrack {
    pub track: Track,
    pub frame_rate: Ratio,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioTrack {
    pub track: Track,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub index: usize,
    pub codec: String,
    pub time_base: Ratio,

    /// One per scanned interval. Intervals aren't contiguous, so no packet interval is measured
    /// across two segments.
    pub segments: Vec<Segment>,
}

/// Packets from one contiguous read, in presentation order.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    packets: Vec<Packet>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    pub pts: i64,
    pub duration: Option<i64>,
}

impl Segment {
    /// Sorts by `pts`, because B-frames make video decode order differ from presentation order.
    pub fn new(mut packets: Vec<Packet>) -> Self {
        packets.sort_unstable_by_key(|p| p.pts);
        Segment { packets }
    }

    fn intervals(&self) -> impl Iterator<Item = i64> + '_ {
        self.packets
            .windows(2)
            .map(|pair| pair[1].pts - pair[0].pts)
    }
}

impl Track {
    pub fn packet_count(&self) -> usize {
        self.segments.iter().map(|s| s.packets.len()).sum()
    }

    fn first(&self) -> Option<&Packet> {
        self.segments.iter().find_map(|s| s.packets.first())
    }

    fn last(&self) -> Option<&Packet> {
        self.segments.iter().rev().find_map(|s| s.packets.last())
    }

    fn nanos(&self, ticks: i64) -> Nanos {
        Nanos::from_scaled_ticks(i128::from(ticks), 1, self.time_base)
    }

    /// Presentation start and end, the end being the last packet's pts plus a final duration of
    /// `num / den` ticks. Adds in ticks so the end rounds once.
    fn span(&self, (num, den): (i128, i128)) -> Option<Span> {
        let end = i128::from(self.last()?.pts) * den + num;
        Some(Span {
            start: self.nanos(self.first()?.pts),
            end: Nanos::from_scaled_ticks(end, den, self.time_base),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: Nanos,
    pub end: Nanos,
}

impl Span {
    fn length(self) -> Nanos {
        self.end - self.start
    }
}

impl VideoTrack {
    /// The nominal frame duration in ticks, as the exact fraction `num / den`.
    fn frame_ticks(&self) -> (i128, i128) {
        let (tb, rate) = (self.track.time_base, self.frame_rate);
        (
            i128::from(tb.den()) * i128::from(rate.den()),
            i128::from(tb.num()) * i128::from(rate.num()),
        )
    }

    fn frame(&self) -> Nanos {
        let (num, den) = self.frame_ticks();
        Nanos::from_scaled_ticks(num, den, self.track.time_base)
    }

    pub fn span(&self) -> Option<Span> {
        self.track.span(self.frame_ticks())
    }
}

impl AudioTrack {
    /// The typical packet duration in ticks: the median of the packets' own durations, or of the
    /// intervals between them when the container doesn't store durations. ffprobe doesn't report
    /// the codec frame size, so this stands in for `frame_size / sample_rate`.
    fn frame_ticks(&self) -> i64 {
        let segments = &self.track.segments;
        let durations = segments
            .iter()
            .flat_map(|s| &s.packets)
            .filter_map(|p| p.duration);
        median(durations.collect())
            .or_else(|| median(segments.iter().flat_map(Segment::intervals).collect()))
            .unwrap_or(0)
    }

    fn frame(&self) -> Nanos {
        self.track.nanos(self.frame_ticks())
    }

    pub fn span(&self) -> Option<Span> {
        self.track.span((i128::from(self.frame_ticks()), 1))
    }
}

fn median(mut values: Vec<i64>) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mid = values.len() / 2;
    Some(*values.select_nth_unstable(mid).1)
}

/// A signed time in nanoseconds. Serializes as seconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Nanos(i64);

impl Nanos {
    pub const fn from_millis(millis: i64) -> Self {
        Nanos(millis * 1_000_000)
    }

    pub fn abs(self) -> Self {
        Nanos(self.0.abs())
    }

    /// Rounds half up to whole milliseconds.
    pub fn round_millis(self) -> i64 {
        (self.0 + 500_000).div_euclid(1_000_000)
    }

    pub fn as_secs_f64(self) -> f64 {
        // Precision loss only matters beyond 104 days.
        #[allow(clippy::cast_precision_loss)]
        let nanos = self.0 as f64;
        nanos / 1e9
    }

    /// Converts `scaled / scale` ticks of `time_base` to nanoseconds, rounding half up.
    fn from_scaled_ticks(scaled: i128, scale: i128, time_base: Ratio) -> Self {
        let num = scaled * i128::from(time_base.num()) * 1_000_000_000;
        let den = scale * i128::from(time_base.den());
        let rounded = (2 * num + den).div_euclid(2 * den);
        Nanos(i64::try_from(rounded).unwrap_or(i64::MAX))
    }
}

impl std::ops::Add for Nanos {
    type Output = Nanos;

    fn add(self, other: Nanos) -> Nanos {
        Nanos(self.0 + other.0)
    }
}

impl std::ops::Sub for Nanos {
    type Output = Nanos;

    fn sub(self, other: Nanos) -> Nanos {
        Nanos(self.0 - other.0)
    }
}

impl std::ops::Mul<i64> for Nanos {
    type Output = Nanos;

    fn mul(self, factor: i64) -> Nanos {
        Nanos(self.0 * factor)
    }
}

impl fmt::Display for Nanos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.abs() < Nanos::from_millis(1000) {
            write!(f, "{:.1} ms", self.as_secs_f64() * 1000.0)
        } else {
            write!(f, "{:.3} s", self.as_secs_f64())
        }
    }
}

impl Serialize for Nanos {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.as_secs_f64())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub stream: usize,
    pub severity: Severity,
    pub action: Action,
    #[serde(flatten)]
    pub kind: FindingKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Desync below [`PERCEPTIBLE`].
    Minor,
    /// Desync at or above [`PERCEPTIBLE`].
    Major,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Rewrite video timestamps to exact frame multiples with the `setts` bitstream filter,
    /// without re-encoding. Valid only for jitter, because it keeps every frame.
    RetimeToCfr,
    /// Re-encode video at a constant rate, duplicating or dropping frames to keep them in place.
    ReencodeCfr,
    /// Insert silence before the first audio packet.
    PadAudioStart,
    /// Cut the audio that plays before the first video frame.
    TrimAudioStart,
    /// Re-encode audio with silence in the gaps. Video is copied.
    FillAudioGaps,
    /// Resample audio to the video's length, which corrects clock drift.
    StretchAudio,
    /// No automatic fix applies.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Media {
    Video,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FindingKind {
    EmptyStream {
        media: Media,
    },
    /// Frame intervals vary while staying under the drop threshold.
    VariableFrameRate {
        intervals: usize,
        /// Intervals more than [`CFR_TOLERANCE_TICKS`] off the nominal frame duration.
        irregular: usize,
        /// Bounds over all intervals except drops.
        shortest: Nanos,
        longest: Nanos,
        /// The furthest any frame sits from where a constant-rate editor would place it,
        /// discounting dropped frames.
        max_drift: Nanos,
    },
    DroppedFrames {
        /// Intervals at or above the drop threshold.
        events: usize,
        frames: i64,
        /// Time those intervals exceed one frame by, which a constant-rate editor removes.
        missing: Nanos,
    },
    /// Audio start minus video start; positive means audio starts late.
    AudioStartOffset {
        offset: Nanos,
    },
    AudioGaps {
        count: usize,
        /// Time those intervals exceed one packet by, which an editor that ignores audio
        /// timestamps removes.
        total: Nanos,
    },
    /// Audio span minus video span; negative means audio is shorter. A start offset shifts both
    /// ends of the span and gaps sit inside it, so neither changes this. What remains is a
    /// truncated tail or clock drift, which timestamps can't tell apart.
    DurationMismatch {
        difference: Nanos,
    },
}

/// Diagnoses sync problems. Findings come in stream order: video first, then each audio track.
pub fn diagnose(probe: &Probe) -> Vec<Finding> {
    let mut findings = Vec::new();
    let video = &probe.video;
    let video_span = video.span();
    if video_span.is_none() {
        findings.push(Finding::empty(video.track.index, Media::Video));
    }
    findings.extend(video_findings(video));

    for audio in &probe.audio {
        let Some(audio_span) = audio.span() else {
            findings.push(Finding::empty(audio.track.index, Media::Audio));
            continue;
        };
        let floor = (video.frame() * VIDEO_NOISE_FLOOR_FRAMES)
            .max(audio.frame() * AUDIO_NOISE_FLOOR_FRAMES);
        let (gaps, gap_total) = audio_gaps(audio);

        if let Some(video_span) = video_span {
            let offset = audio_span.start - video_span.start;
            if offset.abs() >= floor {
                findings.push(Finding::new(
                    audio.track.index,
                    offset,
                    if offset > Nanos::default() {
                        Action::PadAudioStart
                    } else {
                        Action::TrimAudioStart
                    },
                    FindingKind::AudioStartOffset { offset },
                ));
            }
        }

        if gaps > 0 && gap_total >= floor {
            findings.push(Finding::new(
                audio.track.index,
                gap_total,
                Action::FillAudioGaps,
                FindingKind::AudioGaps {
                    count: gaps,
                    total: gap_total,
                },
            ));
        }

        if let Some(video_span) = video_span {
            let difference = audio_span.length() - video_span.length();
            if difference.abs() >= floor {
                findings.push(Finding::new(
                    audio.track.index,
                    difference,
                    Action::StretchAudio,
                    FindingKind::DurationMismatch { difference },
                ));
            }
        }
    }
    findings
}

impl Finding {
    /// Grades severity by `desync`, the sync error the defect causes.
    fn new(stream: usize, desync: Nanos, action: Action, kind: FindingKind) -> Self {
        Finding {
            stream,
            severity: if desync.abs() >= PERCEPTIBLE {
                Severity::Major
            } else {
                Severity::Minor
            },
            action,
            kind,
        }
    }

    fn empty(stream: usize, media: Media) -> Self {
        Finding {
            stream,
            severity: Severity::Major,
            action: Action::None,
            kind: FindingKind::EmptyStream { media },
        }
    }
}

fn video_findings(video: &VideoTrack) -> Vec<Finding> {
    // Intervals are scaled by the frame duration's denominator so the frame is exactly `frame`.
    let (frame, scale) = video.frame_ticks();
    let to_nanos = |scaled| Nanos::from_scaled_ticks(scaled, scale, video.track.time_base);

    let mut intervals = 0;
    let mut irregular = 0;
    let mut bounds: Option<(i128, i128)> = None;
    let mut max_drift = 0;
    let (mut drops, mut dropped_frames, mut missing) = (0, 0, 0);

    for segment in &video.track.segments {
        let Some(first) = segment.packets.first() else {
            continue;
        };
        // Where a constant-rate editor would place the current frame, in frames from `first`.
        let mut slot: i128 = 0;
        for (packet, interval) in segment.packets[1..].iter().zip(segment.intervals()) {
            let interval = i128::from(interval) * scale;
            intervals += 1;
            slot += 1;
            if DROP_FACTOR.reached_by(interval, frame) {
                // Rounds to the nearest whole number of frames.
                let lost = (2 * interval + frame) / (2 * frame) - 1;
                drops += 1;
                dropped_frames += lost;
                missing += interval - frame;
                slot += lost;
            } else {
                if (interval - frame).abs() > i128::from(CFR_TOLERANCE_TICKS) * scale {
                    irregular += 1;
                }
                bounds = Some(bounds.map_or((interval, interval), |(lo, hi)| {
                    (lo.min(interval), hi.max(interval))
                }));
            }
            let position = i128::from(packet.pts - first.pts) * scale;
            max_drift = max_drift.max((position - slot * frame).abs());
        }
    }

    let mut findings = Vec::new();
    if irregular > 0 {
        let max_drift = to_nanos(max_drift);
        let (shortest, longest) = bounds.unwrap_or_default();
        let retimable = drops == 0 && max_drift < video.frame() * VIDEO_NOISE_FLOOR_FRAMES;
        findings.push(Finding::new(
            video.track.index,
            max_drift,
            if retimable {
                Action::RetimeToCfr
            } else {
                Action::ReencodeCfr
            },
            FindingKind::VariableFrameRate {
                intervals,
                irregular,
                shortest: to_nanos(shortest),
                longest: to_nanos(longest),
                max_drift,
            },
        ));
    }
    if drops > 0 {
        let missing = to_nanos(missing);
        findings.push(Finding::new(
            video.track.index,
            missing,
            Action::ReencodeCfr,
            FindingKind::DroppedFrames {
                events: drops,
                frames: i64::try_from(dropped_frames).unwrap_or(i64::MAX),
                missing,
            },
        ));
    }
    findings
}

/// Counts intervals over the gap threshold and the total time they exceed one packet by.
fn audio_gaps(audio: &AudioTrack) -> (usize, Nanos) {
    let frame = i128::from(audio.frame_ticks());
    let (count, total) = audio
        .track
        .segments
        .iter()
        .flat_map(Segment::intervals)
        .map(i128::from)
        .filter(|&interval| interval * GAP_FACTOR.den > frame * GAP_FACTOR.num)
        .fold((0, 0), |(count, total), interval| {
            (count + 1, total + interval - frame)
        });
    (
        count,
        Nanos::from_scaled_ticks(total, 1, audio.track.time_base),
    )
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Minor => "minor",
            Severity::Major => "major",
        })
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Action::RetimeToCfr => "retime video to a constant frame rate (no re-encode)",
            Action::ReencodeCfr => "re-encode video at a constant frame rate",
            Action::PadAudioStart => "pad the start of the audio with silence",
            Action::TrimAudioStart => "trim the audio before the first video frame",
            Action::FillAudioGaps => "fill the audio gaps with silence (audio-only re-encode)",
            Action::StretchAudio => {
                "stretch audio to the video's length if it drifts out of sync toward the end; \
                 a truncated tail needs no fix"
            }
            Action::None => "none available",
        })
    }
}

impl fmt::Display for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Media::Video => "video",
            Media::Audio => "audio",
        })
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stream = self.stream;
        write!(f, "{}: ", self.severity)?;
        match &self.kind {
            FindingKind::EmptyStream { media } => write!(f, "{media} #{stream} has no packets")?,
            FindingKind::VariableFrameRate {
                intervals,
                irregular,
                shortest,
                longest,
                max_drift,
            } => write!(
                f,
                "video #{stream} frame intervals vary from {shortest} to {longest} \
                 ({irregular} of {intervals} off nominal), drifting up to {max_drift} \
                 from constant-rate positions"
            )?,
            FindingKind::DroppedFrames {
                events,
                frames,
                missing,
            } => write!(
                f,
                "video #{stream} dropped {frames} frame(s) at {events} point(s), \
                 {missing} missing"
            )?,
            FindingKind::AudioStartOffset { offset } => write!(
                f,
                "audio #{stream} starts {} {} video",
                offset.abs(),
                if *offset > Nanos::default() {
                    "after"
                } else {
                    "before"
                }
            )?,
            FindingKind::AudioGaps { count, total } => {
                write!(f, "audio #{stream} has {count} gap(s) totalling {total}")?;
            }
            FindingKind::DurationMismatch { difference } => write!(
                f,
                "audio #{stream} spans {} {} than video (truncated tail or clock drift)",
                difference.abs(),
                if *difference < Nanos::default() {
                    "less"
                } else {
                    "more"
                }
            )?,
        }
        write!(f, ". Fix: {}.", self.action)
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::*;

    /// 60 fps in a 1/90000 time base, as in MP4: 1500 ticks per frame.
    const FRAME: i64 = 1500;

    /// AAC at 48 kHz in a 1/48000 time base: 1024 ticks per packet.
    const AAC: i64 = 1024;

    fn ratio(num: i64, den: i64) -> Ratio {
        Ratio::new(num, den).unwrap()
    }

    fn frames(slots: Range<i64>) -> Vec<i64> {
        slots.map(|slot| slot * FRAME).collect()
    }

    fn aac(slots: Range<i64>) -> Vec<i64> {
        slots.map(|slot| slot * AAC).collect()
    }

    fn track(
        index: usize,
        time_base: Ratio,
        segments: Vec<Vec<i64>>,
        duration: Option<i64>,
    ) -> Track {
        Track {
            index,
            codec: "test".into(),
            time_base,
            segments: segments
                .into_iter()
                .map(|pts| {
                    Segment::new(
                        pts.into_iter()
                            .map(|pts| Packet { pts, duration })
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    fn video(segments: Vec<Vec<i64>>) -> VideoTrack {
        VideoTrack {
            track: track(0, ratio(1, 90_000), segments, Some(FRAME)),
            frame_rate: ratio(60, 1),
        }
    }

    fn audio(index: usize, segments: Vec<Vec<i64>>) -> AudioTrack {
        AudioTrack {
            track: track(index, ratio(1, 48_000), segments, Some(AAC)),
            sample_rate: 48_000,
        }
    }

    /// Ten seconds of video, and audio that matches it to within the final packet.
    fn clean_video() -> VideoTrack {
        video(vec![frames(0..600)])
    }

    fn clean_audio(index: usize) -> AudioTrack {
        audio(index, vec![aac(0..469)])
    }

    fn diagnose_streams(video: VideoTrack, audio: Vec<AudioTrack>) -> Vec<Finding> {
        diagnose(&Probe { video, audio })
    }

    #[test]
    fn clean_streams_have_no_findings() {
        assert_eq!(
            diagnose_streams(clean_video(), vec![clean_audio(1)]),
            vec![]
        );
    }

    #[test]
    fn millisecond_rounding_of_constant_rate_is_regular() {
        // 60 fps in Matroska's 1/1000 time base: intervals of 16 and 17 ticks.
        let pts = (0..600).map(|slot| (slot * 1000 + 30) / 60).collect();
        let video = VideoTrack {
            track: track(0, ratio(1, 1000), vec![pts], None),
            frame_rate: ratio(60, 1),
        };

        assert_eq!(diagnose_streams(video, vec![]), vec![]);
    }

    #[test]
    fn reorders_b_frame_packets_before_measuring() {
        let decode_order = vec![0, 4500, 1500, 3000, 9000, 6000, 7500];

        assert_eq!(diagnose_streams(video(vec![decode_order]), vec![]), vec![]);
    }

    #[test]
    fn jitter_without_drift_retimes_to_cfr() {
        // Frames sit up to 45 ticks (0.5 ms) off their slots, but never accumulate.
        let pts = vec![0, 1545, 2955, 4500, 6045, 7455, 9000];

        assert_eq!(
            diagnose_streams(video(vec![pts]), vec![]),
            vec![Finding {
                stream: 0,
                severity: Severity::Minor,
                action: Action::RetimeToCfr,
                kind: FindingKind::VariableFrameRate {
                    intervals: 6,
                    irregular: 6,
                    shortest: Nanos(15_666_667),
                    longest: Nanos(17_166_667),
                    max_drift: Nanos(500_000),
                },
            }]
        );
    }

    #[test]
    fn jitter_with_accumulated_drift_reencodes() {
        // Labelled 60 fps but stamped 25 ticks slow per frame, so frame 199 lands 4975 ticks
        // (55.3 ms) late. Retiming would move it that far.
        let pts = (0..200).map(|slot| slot * 1525).collect();

        assert_eq!(
            diagnose_streams(video(vec![pts]), vec![]),
            vec![Finding {
                stream: 0,
                severity: Severity::Major,
                action: Action::ReencodeCfr,
                kind: FindingKind::VariableFrameRate {
                    intervals: 199,
                    irregular: 199,
                    shortest: Nanos(16_944_444),
                    longest: Nanos(16_944_444),
                    max_drift: Nanos(55_277_778),
                },
            }]
        );
    }

    #[test]
    fn reports_dropped_frames_without_jitter() {
        // Frame 3 alone and frames 6-7 together are missing.
        let pts = vec![0, 1500, 3000, 6000, 7500, 12000, 13500];

        assert_eq!(
            diagnose_streams(video(vec![pts]), vec![]),
            vec![Finding {
                stream: 0,
                severity: Severity::Major,
                action: Action::ReencodeCfr,
                kind: FindingKind::DroppedFrames {
                    events: 2,
                    frames: 3,
                    missing: Nanos(50_000_000),
                },
            }]
        );
    }

    #[test]
    fn grades_dropped_frames_by_perceptible_desync() {
        let dropping = |interval| video(vec![vec![0, interval, interval + FRAME]]);
        let severity = |interval| diagnose_streams(dropping(interval), vec![])[0].severity;

        // An interval of 5550 ticks loses 4050 ticks, exactly 45 ms.
        assert_eq!(severity(5549), Severity::Minor);
        assert_eq!(severity(5550), Severity::Major);
    }

    #[test]
    fn jitter_with_drops_reencodes_and_discounts_drops_from_drift() {
        // Slot 3 is dropped. The others sit up to 45 ticks off their slots.
        let pts = vec![0, 1545, 2955, 6000, 7545];

        assert_eq!(
            diagnose_streams(video(vec![pts]), vec![]),
            vec![
                Finding {
                    stream: 0,
                    severity: Severity::Minor,
                    action: Action::ReencodeCfr,
                    kind: FindingKind::VariableFrameRate {
                        intervals: 4,
                        irregular: 3,
                        shortest: Nanos(15_666_667),
                        longest: Nanos(17_166_667),
                        max_drift: Nanos(500_000),
                    },
                },
                Finding {
                    stream: 0,
                    severity: Severity::Minor,
                    action: Action::ReencodeCfr,
                    kind: FindingKind::DroppedFrames {
                        events: 1,
                        frames: 1,
                        missing: Nanos(17_166_667),
                    },
                },
            ]
        );
    }

    #[test]
    fn late_audio_start_pads() {
        let late = audio(
            1,
            vec![aac(0..469).into_iter().map(|t| t + 24_000).collect()],
        );

        assert_eq!(
            diagnose_streams(clean_video(), vec![late]),
            vec![Finding {
                stream: 1,
                severity: Severity::Major,
                action: Action::PadAudioStart,
                kind: FindingKind::AudioStartOffset {
                    offset: Nanos(500_000_000),
                },
            }]
        );
    }

    #[test]
    fn early_audio_start_trims() {
        let early = audio(
            1,
            vec![aac(0..469).into_iter().map(|t| t - 24_000).collect()],
        );

        assert_eq!(
            diagnose_streams(clean_video(), vec![early]),
            vec![Finding {
                stream: 1,
                severity: Severity::Major,
                action: Action::TrimAudioStart,
                kind: FindingKind::AudioStartOffset {
                    offset: Nanos(-500_000_000),
                },
            }]
        );
    }

    #[test]
    fn ignores_audio_offsets_under_two_audio_packets() {
        // The floor is two AAC packets (2048 ticks, 42.7 ms), longer than one video frame.
        let shifted = |offset| {
            let pts = aac(0..469).into_iter().map(|t| t + offset).collect();
            diagnose_streams(clean_video(), vec![audio(1, vec![pts])])
        };

        assert_eq!(shifted(2047), vec![]);
        assert_eq!(
            shifted(2048),
            vec![Finding {
                stream: 1,
                severity: Severity::Minor,
                action: Action::PadAudioStart,
                kind: FindingKind::AudioStartOffset {
                    offset: Nanos(42_666_667),
                },
            }]
        );
    }

    #[test]
    fn audio_gaps_fill_without_a_duration_mismatch() {
        // Packets 200-223 are missing, a 24-packet (512 ms) hole inside an intact span.
        let pts = aac(0..200).into_iter().chain(aac(224..469)).collect();

        assert_eq!(
            diagnose_streams(clean_video(), vec![audio(1, vec![pts])]),
            vec![Finding {
                stream: 1,
                severity: Severity::Major,
                action: Action::FillAudioGaps,
                kind: FindingKind::AudioGaps {
                    count: 1,
                    total: Nanos(512_000_000),
                },
            }]
        );
    }

    #[test]
    fn measures_gaps_from_intervals_when_packets_lack_durations() {
        let pts = aac(0..200).into_iter().chain(aac(224..469)).collect();
        let without_durations = AudioTrack {
            track: track(1, ratio(1, 48_000), vec![pts], None),
            sample_rate: 48_000,
        };

        assert_eq!(
            diagnose_streams(clean_video(), vec![without_durations]),
            vec![Finding {
                stream: 1,
                severity: Severity::Major,
                action: Action::FillAudioGaps,
                kind: FindingKind::AudioGaps {
                    count: 1,
                    total: Nanos(512_000_000),
                },
            }]
        );
    }

    #[test]
    fn short_audio_tail_is_a_duration_mismatch() {
        // 422 packets end at 432128 ticks (9.002667 s) against 10 s of video.
        assert_eq!(
            diagnose_streams(clean_video(), vec![audio(1, vec![aac(0..422)])]),
            vec![Finding {
                stream: 1,
                severity: Severity::Major,
                action: Action::StretchAudio,
                kind: FindingKind::DurationMismatch {
                    difference: Nanos(-997_333_333),
                },
            }]
        );
    }

    #[test]
    fn reports_each_audio_track_by_stream_index() {
        let late = audio(
            2,
            vec![aac(0..469).into_iter().map(|t| t + 24_000).collect()],
        );

        assert_eq!(
            diagnose_streams(clean_video(), vec![clean_audio(1), late]),
            vec![Finding {
                stream: 2,
                severity: Severity::Major,
                action: Action::PadAudioStart,
                kind: FindingKind::AudioStartOffset {
                    offset: Nanos(500_000_000),
                },
            }]
        );
    }

    #[test]
    fn does_not_measure_across_sampled_segments() {
        // A head and a tail sample: frames 0-9 and 1000-1009, audio packets 0-9 and 780-788.
        let video = video(vec![frames(0..10), frames(1000..1010)]);
        let audio = audio(1, vec![aac(0..10), aac(780..789)]);

        assert_eq!(diagnose_streams(video, vec![audio]), vec![]);
    }

    #[test]
    fn empty_video_skips_audio_comparisons() {
        assert_eq!(
            diagnose_streams(video(vec![vec![]]), vec![clean_audio(1)]),
            vec![Finding {
                stream: 0,
                severity: Severity::Major,
                action: Action::None,
                kind: FindingKind::EmptyStream {
                    media: Media::Video,
                },
            }]
        );
    }

    #[test]
    fn reports_empty_audio() {
        assert_eq!(
            diagnose_streams(clean_video(), vec![audio(1, vec![vec![]])]),
            vec![Finding {
                stream: 1,
                severity: Severity::Major,
                action: Action::None,
                kind: FindingKind::EmptyStream {
                    media: Media::Audio,
                },
            }]
        );
    }

    #[test]
    fn video_without_audio_is_checked_alone() {
        assert_eq!(diagnose_streams(clean_video(), vec![]), vec![]);
    }

    #[test]
    fn single_frame_and_packet_have_no_findings() {
        assert_eq!(
            diagnose_streams(video(vec![vec![0]]), vec![audio(1, vec![vec![0]])]),
            vec![]
        );
    }

    #[test]
    fn spans_end_one_frame_after_the_last_packet() {
        assert_eq!(
            clean_video().span(),
            Some(Span {
                start: Nanos(0),
                end: Nanos(10_000_000_000),
            })
        );
        assert_eq!(
            clean_audio(1).span(),
            Some(Span {
                start: Nanos(0),
                end: Nanos(10_005_333_333),
            })
        );
    }

    #[test]
    fn formats_nanos_in_milliseconds_below_a_second() {
        assert_eq!(Nanos(16_666_667).to_string(), "16.7 ms");
        assert_eq!(Nanos(-500_000_000).to_string(), "-500.0 ms");
        assert_eq!(Nanos(1_800_000_000).to_string(), "1.800 s");
    }

    #[test]
    fn describes_findings_with_their_fix() {
        let finding = Finding {
            stream: 1,
            severity: Severity::Major,
            action: Action::PadAudioStart,
            kind: FindingKind::AudioStartOffset {
                offset: Nanos(500_000_000),
            },
        };

        assert_eq!(
            finding.to_string(),
            "major: audio #1 starts 500.0 ms after video. \
             Fix: pad the start of the audio with silence."
        );
    }
}
