//! Typed wrapper around `ffprobe`.
//!
//! Private `Raw*` structs mirror ffprobe's JSON exactly; public types are what tools consume.

use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use tokio::process::Command;

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
