//! Runs ffmpeg.

use std::{ffi::OsString, path::Path, process::Stdio};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// How [`extract_audio`] treats the stream's channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    Keep,
    Mono,
}

/// Decodes stream `index` of `input` into a float WAV at `sample_rate`.
///
/// RF64 lifts WAV's 4 GB limit, which float32 stereo passes after about 3 hours at 48 kHz.
pub async fn extract_audio(
    input: &Path,
    index: usize,
    sample_rate: u32,
    channels: Channels,
    out: &Path,
) -> Result<()> {
    let mut args: Vec<OsString> = vec![
        "-i".into(),
        input.into(),
        "-map".into(),
        format!("0:{index}").into(),
        "-ar".into(),
        sample_rate.to_string().into(),
    ];
    if channels == Channels::Mono {
        args.extend(["-ac".into(), "1".into()]);
    }
    args.extend([
        "-c:a".into(),
        "pcm_f32le".into(),
        "-rf64".into(),
        "auto".into(),
        out.into(),
    ]);
    run(args, "extract audio").await
}

/// Runs ffmpeg quietly, overwriting outputs. `what` names the step in the error.
pub async fn run(args: impl IntoIterator<Item = OsString>, what: &str) -> Result<()> {
    let output = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(args)
        .stdin(Stdio::null())
        // Cancelling the future kills ffmpeg.
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to run ffmpeg; is it installed and on PATH?")?;
    if !output.status.success() {
        bail!(
            "ffmpeg failed to {what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
