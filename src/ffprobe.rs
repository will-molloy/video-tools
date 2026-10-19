//! Typed wrapper around `ffprobe`.
//!
//! Private `Raw*` structs mirror ffprobe's JSON exactly; public types are what tools consume.

use std::{fmt, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;

/// A chapter marker stored in the container.
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub start: Duration,
    pub end: Duration,
    pub title: Option<String>,
}

/// A file's chapter markers, in container order, plus its overall duration.
#[derive(Debug, Clone, PartialEq)]
pub struct Chapters {
    pub chapters: Vec<Chapter>,

    /// ffprobe reports `N/A` for some inputs, such as unfinalised recordings.
    pub duration: Option<Duration>,
}

pub async fn chapters(path: &Path) -> Result<Chapters> {
    let stdout = run(
        path,
        &["-show_chapters", "-show_entries", "format=duration"],
    )
    .await?;
    parse_chapters(&stdout)
}

fn parse_chapters(json: &[u8]) -> Result<Chapters> {
    let raw: RawChapters =
        serde_json::from_slice(json).context("failed to parse ffprobe chapters")?;
    Ok(raw.into())
}

/// A positive exact fraction, such as a time base of `1/90000` or a frame rate of `30000/1001`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio {
    num: i64,
    den: i64,
}

impl Ratio {
    /// Returns `None` unless both parts are positive, which also rejects ffprobe's `0/0`.
    pub fn new(num: i64, den: i64) -> Option<Self> {
        (num > 0 && den > 0).then_some(Ratio { num, den })
    }

    pub fn num(self) -> i64 {
        self.num
    }

    pub fn den(self) -> i64 {
        self.den
    }

    fn parse(text: &str) -> Option<Self> {
        let (num, den) = text.split_once('/')?;
        Ratio::new(num.parse().ok()?, den.parse().ok()?)
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

/// A file's streams, in container order, plus its overall duration.
#[derive(Debug, Clone, PartialEq)]
pub struct Streams {
    pub streams: Vec<Stream>,
    pub duration: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub index: usize,
    pub codec: String,

    /// The unit of the stream's packet timestamps, in seconds.
    pub time_base: Ratio,

    /// Containers that index their packets (MP4) report this; Matroska doesn't.
    pub packet_count: Option<u64>,

    pub kind: StreamKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamKind {
    Video {
        /// ffprobe's `r_frame_rate`, the base rate all timestamps are multiples of. `None` when
        /// ffprobe can't guess one.
        frame_rate: Option<Ratio>,
    },
    Audio {
        sample_rate: u32,
    },

    /// Subtitles, data, attachments, and cover art (an attached picture is a one-frame video
    /// stream in ffprobe's model).
    Other,
}

pub async fn streams(path: &Path) -> Result<Streams> {
    let stdout = run(
        path,
        &[
            "-show_entries",
            "stream=index,codec_type,codec_name,time_base,r_frame_rate,sample_rate,nb_frames\
             :stream_disposition=attached_pic:format=duration",
        ],
    )
    .await?;
    parse_streams(&stdout)
}

fn parse_streams(json: &[u8]) -> Result<Streams> {
    let raw: RawStreams =
        serde_json::from_slice(json).context("failed to parse ffprobe streams")?;
    let streams = raw
        .streams
        .into_iter()
        .map(Stream::try_from)
        .collect::<Result<_>>()?;
    Ok(Streams {
        streams,
        duration: raw.format.and_then(|f| f.duration),
    })
}

/// One demuxed packet. Packets arrive in decode order, so video `pts` can go backwards when the
/// stream has B-frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    pub stream: usize,

    /// Presentation timestamp, in the stream's time base.
    pub pts: i64,

    pub duration: Option<i64>,

    /// Set on packets the decoder drops after decoding, such as the encoder-delay priming frame
    /// MP4 stores before an AAC stream's first audible sample.
    pub discard: bool,
}

/// The part of the file a packet scan reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadInterval {
    Whole,

    /// From the start of the file for this long.
    Head(Duration),

    /// From this timestamp to the end of the file. ffprobe seeks to the keyframe at or before it.
    From(Duration),
}

impl ReadInterval {
    fn ffprobe_arg(self) -> Option<String> {
        match self {
            ReadInterval::Whole => None,
            ReadInterval::Head(length) => Some(format!("%+{:.3}", length.as_secs_f64())),
            ReadInterval::From(start) => Some(format!("{:.3}%", start.as_secs_f64())),
        }
    }
}

/// Returned by [`packets`] when the scan is cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Streams every packet in `interval` to `on_packet`, one ffprobe output line at a time, so
/// memory stays flat however large the file is.
///
/// Cancelling `cancel` kills ffprobe, waits for it to exit, and returns [`Cancelled`].
pub async fn packets(
    path: &Path,
    interval: ReadInterval,
    cancel: &CancellationToken,
    mut on_packet: impl FnMut(Packet),
) -> Result<()> {
    let mut child = Command::new("ffprobe")
        .args(["-v", "error", "-of", "compact=p=0"])
        .args(["-show_entries", "packet=stream_index,pts,duration,flags"])
        .args(
            interval
                .ffprobe_arg()
                .map(|arg| ["-read_intervals".to_string(), arg])
                .into_iter()
                .flatten(),
        )
        .arg("-i")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Backstop for the early returns below. Cancellation kills explicitly so it can also reap
        // the process.
        .kill_on_drop(true)
        .spawn()
        .context("failed to run ffprobe; is it installed and on PATH?")?;

    // Drain stderr concurrently. A pipe nobody reads fills up and blocks ffprobe mid-write.
    let mut stderr = child.stderr.take().context("ffprobe stderr not captured")?;
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await.map(|_| buf)
    });

    let mut stdout = BufReader::new(child.stdout.take().context("ffprobe stdout not captured")?);
    let mut line = String::new();

    // Created once and polled by reference, so each loop iteration doesn't register a new waiter.
    let cancelled = cancel.cancelled();
    tokio::pin!(cancelled);

    loop {
        line.clear();
        // `biased` polls cancellation first rather than in random order, so a steady stream of
        // ready lines can't delay it. `read_line` loses partial input when the other branch wins,
        // which doesn't matter because cancelling abandons the output.
        let read = tokio::select! {
            biased;
            () = &mut cancelled => {
                child.kill().await.context("failed to stop ffprobe")?;
                return Err(Cancelled.into());
            }
            read = stdout.read_line(&mut line) => read.context("failed to read ffprobe output")?,
        };
        if read == 0 {
            break;
        }
        if let Some(packet) = parse_packet(line.trim_end())? {
            on_packet(packet);
        }
    }

    let status = child.wait().await.context("failed to wait for ffprobe")?;
    let stderr = stderr_task
        .await
        .context("ffprobe stderr reader panicked")?
        .context("failed to read ffprobe stderr")?;
    if !status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(())
}

/// Parses a `compact` line such as `stream_index=0|pts=1500|duration=1500|flags=K__`.
/// Returns `None` for packets without a timestamp, which can't be placed on the timeline.
fn parse_packet(line: &str) -> Result<Option<Packet>> {
    let (mut stream, mut pts, mut duration, mut discard) = (None, None, None, false);
    for field in line.split('|') {
        // Packets with side data end in an empty field.
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "stream_index" => stream = value.parse().ok(),
            "pts" => pts = value.parse().ok(),
            "duration" => duration = value.parse().ok(),
            "flags" => discard = value.contains('D'),
            _ => {}
        }
    }
    let stream = stream.with_context(|| format!("ffprobe packet without a stream: {line:?}"))?;
    Ok(pts.map(|pts| Packet {
        stream,
        pts,
        duration,
        discard,
    }))
}

async fn run(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-of", "json"])
        .args(args)
        // `-i` stops a file name starting with `-` from parsing as an option.
        .arg("-i")
        .arg(path)
        // Keep ffprobe off the terminal so it can't steal TUI keystrokes.
        .stdin(Stdio::null())
        // Cancelling the future (e.g. leaving the TUI's running screen) kills the process.
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to run ffprobe; is it installed and on PATH?")?;

    // ffprobe's own message normally names the file.
    if !output.status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

#[derive(Deserialize)]
struct RawChapters {
    #[serde(default)]
    chapters: Vec<RawChapter>,
    format: Option<RawFormat>,
}

#[derive(Deserialize)]
struct RawChapter {
    #[serde(deserialize_with = "seconds")]
    start_time: Duration,
    #[serde(deserialize_with = "seconds")]
    end_time: Duration,
    #[serde(default)]
    tags: RawTags,
}

#[derive(Deserialize, Default)]
struct RawTags {
    title: Option<String>,
}

#[derive(Deserialize)]
struct RawFormat {
    #[serde(default, deserialize_with = "optional_seconds")]
    duration: Option<Duration>,
}

#[derive(Deserialize)]
struct RawStreams {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: Option<RawFormat>,
}

#[derive(Deserialize)]
struct RawStream {
    index: usize,
    codec_type: Option<String>,
    codec_name: Option<String>,
    time_base: String,
    r_frame_rate: Option<String>,
    sample_rate: Option<String>,
    nb_frames: Option<String>,
    #[serde(default)]
    disposition: RawDisposition,
}

#[derive(Deserialize, Default)]
struct RawDisposition {
    #[serde(default)]
    attached_pic: u8,
}

impl TryFrom<RawStream> for Stream {
    type Error = anyhow::Error;

    fn try_from(raw: RawStream) -> Result<Self> {
        let time_base = Ratio::parse(&raw.time_base).with_context(|| {
            format!(
                "stream {} has invalid time base {:?}",
                raw.index, raw.time_base
            )
        })?;
        let kind = match raw.codec_type.as_deref() {
            Some("video") if raw.disposition.attached_pic == 0 => StreamKind::Video {
                frame_rate: raw.r_frame_rate.as_deref().and_then(Ratio::parse),
            },
            Some("audio") => StreamKind::Audio {
                sample_rate: raw
                    .sample_rate
                    .as_deref()
                    .and_then(|rate| rate.parse().ok())
                    .with_context(|| format!("audio stream {} has no sample rate", raw.index))?,
            },
            _ => StreamKind::Other,
        };
        Ok(Stream {
            index: raw.index,
            codec: raw.codec_name.unwrap_or_else(|| "unknown".into()),
            time_base,
            packet_count: raw.nb_frames.and_then(|n| n.parse().ok()),
            kind,
        })
    }
}

impl From<RawChapters> for Chapters {
    fn from(raw: RawChapters) -> Self {
        Chapters {
            chapters: raw
                .chapters
                .into_iter()
                .map(|c| Chapter {
                    start: c.start_time,
                    end: c.end_time,
                    title: c.tags.title,
                })
                .collect(),
            duration: raw.format.and_then(|f| f.duration),
        }
    }
}

/// ffprobe encodes times as decimal strings, e.g. `"30.500000"`.
fn seconds<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse_seconds(&text).ok_or_else(|| serde::de::Error::custom(format!("invalid time {text:?}")))
}

fn optional_seconds<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    let text = Option::<String>::deserialize(deserializer)?;
    Ok(text.as_deref().and_then(parse_seconds))
}

/// Clamps negative times to zero. Containers occasionally carry tiny negative offsets.
fn parse_seconds(text: &str) -> Option<Duration> {
    let secs: f64 = text.parse().ok()?;
    Duration::try_from_secs_f64(secs.max(0.0)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chapters_and_duration() {
        let json = br#"{
            "chapters": [
                { "id": 1, "time_base": "1/1000", "start": 0, "start_time": "0.000000",
                  "end": 30500, "end_time": "30.500000", "tags": { "title": "Intro" } },
                { "id": 2, "time_base": "1/1000", "start": 30500, "start_time": "30.500000",
                  "end": 65000, "end_time": "65.000000" }
            ],
            "format": { "duration": "65.000000" }
        }"#;

        let parsed = parse_chapters(json).unwrap();

        assert_eq!(
            parsed,
            Chapters {
                chapters: vec![
                    Chapter {
                        start: Duration::ZERO,
                        end: Duration::from_millis(30_500),
                        title: Some("Intro".into()),
                    },
                    Chapter {
                        start: Duration::from_millis(30_500),
                        end: Duration::from_secs(65),
                        title: None,
                    },
                ],
                duration: Some(Duration::from_secs(65)),
            }
        );
    }

    #[test]
    fn parses_streams() {
        let json = br#"{
            "programs": [], "stream_groups": [],
            "streams": [
                { "index": 0, "codec_name": "h264", "codec_type": "video", "r_frame_rate": "60/1",
                  "time_base": "1/1000", "disposition": { "attached_pic": 0 } },
                { "index": 1, "codec_name": "aac", "codec_type": "audio", "sample_rate": "48000",
                  "r_frame_rate": "0/0", "time_base": "1/48000", "nb_frames": "470",
                  "disposition": { "attached_pic": 0 } },
                { "index": 2, "codec_name": "mjpeg", "codec_type": "video",
                  "r_frame_rate": "90000/1", "time_base": "1/90000",
                  "disposition": { "attached_pic": 1 } },
                { "index": 3, "codec_name": "vfr", "codec_type": "video", "r_frame_rate": "0/0",
                  "time_base": "1/90000", "disposition": { "attached_pic": 0 } }
            ],
            "format": { "duration": "10.021000" }
        }"#;

        assert_eq!(
            parse_streams(json).unwrap(),
            Streams {
                streams: vec![
                    Stream {
                        index: 0,
                        codec: "h264".into(),
                        time_base: Ratio::new(1, 1000).unwrap(),
                        packet_count: None,
                        kind: StreamKind::Video {
                            frame_rate: Ratio::new(60, 1),
                        },
                    },
                    Stream {
                        index: 1,
                        codec: "aac".into(),
                        time_base: Ratio::new(1, 48_000).unwrap(),
                        packet_count: Some(470),
                        kind: StreamKind::Audio {
                            sample_rate: 48_000
                        },
                    },
                    Stream {
                        index: 2,
                        codec: "mjpeg".into(),
                        time_base: Ratio::new(1, 90_000).unwrap(),
                        packet_count: None,
                        kind: StreamKind::Other,
                    },
                    Stream {
                        index: 3,
                        codec: "vfr".into(),
                        time_base: Ratio::new(1, 90_000).unwrap(),
                        packet_count: None,
                        kind: StreamKind::Video { frame_rate: None },
                    },
                ],
                duration: Some(Duration::from_millis(10_021)),
            }
        );
    }

    #[test]
    fn parses_packets() {
        assert_eq!(
            parse_packet("stream_index=0|pts=-3000|duration=1500|flags=K__").unwrap(),
            Some(Packet {
                stream: 0,
                pts: -3000,
                duration: Some(1500),
                discard: false,
            })
        );
        // AAC priming, with the empty trailing field ffprobe adds for side data.
        assert_eq!(
            parse_packet("stream_index=1|pts=-1024|duration=1024|flags=KD_|").unwrap(),
            Some(Packet {
                stream: 1,
                pts: -1024,
                duration: Some(1024),
                discard: true,
            })
        );
        assert_eq!(
            parse_packet("stream_index=0|pts=21|duration=N/A|flags=___").unwrap(),
            Some(Packet {
                stream: 0,
                pts: 21,
                duration: None,
                discard: false,
            })
        );
    }

    #[test]
    fn skips_packets_without_a_timestamp() {
        assert_eq!(
            parse_packet("stream_index=0|pts=N/A|duration=1500|flags=K__").unwrap(),
            None
        );
    }

    #[test]
    fn formats_read_intervals() {
        assert_eq!(ReadInterval::Whole.ffprobe_arg(), None);
        assert_eq!(
            ReadInterval::Head(Duration::from_secs(600)).ffprobe_arg(),
            Some("%+600.000".into())
        );
        assert_eq!(
            ReadInterval::From(Duration::from_millis(4_470_500)).ffprobe_arg(),
            Some("4470.500%".into())
        );
    }

    #[test]
    fn parses_unknown_duration_as_none() {
        let json = br#"{ "chapters": [], "format": { "duration": "N/A" } }"#;

        let parsed = parse_chapters(json).unwrap();

        assert_eq!(
            parsed,
            Chapters {
                chapters: vec![],
                duration: None,
            }
        );
    }
}
