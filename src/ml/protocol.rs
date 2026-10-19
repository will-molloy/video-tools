//! The JSON lines an ML stage writes to stdout. `ml/README.md` documents them.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// `R` is the stage's result, which `done` carries alongside the stats every stage reports.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "lowercase",
    bound(deserialize = "R: DeserializeOwned")
)]
pub enum Message<R> {
    Progress { done: u64, total: u64 },
    Log { message: String },
    Done(Done<R>),
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(bound(deserialize = "R: DeserializeOwned"))]
pub struct Done<R> {
    #[serde(flatten)]
    pub result: R,
    #[serde(flatten)]
    pub stats: Stats,
}

/// How a stage ran.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Stats {
    pub device: String,
    pub elapsed_s: f64,

    /// `None` on CPU.
    pub peak_vram_mb: Option<f64>,
}

/// Any line that isn't one of the messages is an error. The stage reserves stdout for the
/// protocol, so a stray line means something is badly wrong.
pub fn parse<R: DeserializeOwned>(line: &str) -> Result<Message<R>> {
    serde_json::from_str(line).with_context(|| format!("vt-ml wrote a malformed line: {line:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    struct Word {
        word: String,
    }

    fn parse(line: &str) -> Result<Message<Word>> {
        super::parse(line)
    }

    #[test]
    fn parses_progress() {
        assert_eq!(
            parse(r#"{"type": "progress", "done": 3, "total": 8}"#).unwrap(),
            Message::Progress { done: 3, total: 8 }
        );
    }

    #[test]
    fn parses_log() {
        assert_eq!(
            parse(r#"{"type": "log", "message": "Loading openai/whisper-large-v3-turbo"}"#)
                .unwrap(),
            Message::Log {
                message: "Loading openai/whisper-large-v3-turbo".into()
            }
        );
    }

    #[test]
    fn parses_done_with_the_result_on_gpu() {
        let line = r#"{"type": "done", "word": "hello",
            "device": "cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2)", "elapsed_s": 12.346,
            "peak_vram_mb": 9123.5}"#;

        assert_eq!(
            parse(line).unwrap(),
            Message::Done(Done {
                result: Word {
                    word: "hello".into()
                },
                stats: Stats {
                    device: "cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2)".into(),
                    elapsed_s: 12.346,
                    peak_vram_mb: Some(9123.5),
                },
            })
        );
    }

    #[test]
    fn parses_done_on_cpu() {
        let line = r#"{"type": "done", "word": "hi", "device": "cpu", "elapsed_s": 1.0,
            "peak_vram_mb": null}"#;

        assert_eq!(
            parse(line).unwrap(),
            Message::Done(Done {
                result: Word { word: "hi".into() },
                stats: Stats {
                    device: "cpu".into(),
                    elapsed_s: 1.0,
                    peak_vram_mb: None,
                },
            })
        );
    }

    #[test]
    fn parses_error() {
        assert_eq!(
            parse(r#"{"type": "error", "message": "the model is gated"}"#).unwrap(),
            Message::Error {
                message: "the model is gated".into()
            }
        );
    }

    #[test]
    fn rejects_lines_that_are_not_json() {
        let error = parse("Downloading model.safetensors: 12%").unwrap_err();

        assert_eq!(
            error.to_string(),
            r#"vt-ml wrote a malformed line: "Downloading model.safetensors: 12%""#
        );
    }

    #[test]
    fn rejects_a_done_without_the_result() {
        assert!(
            parse(r#"{"type": "done", "device": "cpu", "elapsed_s": 1.0, "peak_vram_mb": null}"#)
                .is_err()
        );
    }

    #[test]
    fn rejects_unknown_message_types() {
        assert!(parse(r#"{"type": "warning", "message": "hm"}"#).is_err());
    }

    #[test]
    fn rejects_truncated_lines() {
        assert!(parse(r#"{"type": "progress", "done": 3, "tot"#).is_err());
    }
}
