//! Runs ffmpeg.

use std::{ffi::OsString, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::process;

/// Decodes stream `index` of `input` into a float WAV at `sample_rate`, downmixed to `channels`
/// when given.
///
/// RF64 lifts WAV's 4 GB limit, which float32 stereo passes after about 3 hours at 48 kHz.
pub async fn extract_audio(
    input: &Path,
    index: usize,
    sample_rate: u32,
    channels: Option<u16>,
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
    if let Some(channels) = channels {
        args.extend(["-ac".into(), channels.to_string().into()]);
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

/// Runs the `loudnorm` filter's analysis over stream `index` of `input`, and returns ffmpeg's log,
/// which holds the filter's measurements as JSON. Reports how far into the stream ffmpeg has read
/// to `on_time`.
///
/// Cancelling `cancel` kills ffmpeg, waits for it to exit, and returns [`Cancelled`].
///
/// [`Cancelled`]: crate::ffprobe::Cancelled
pub async fn loudnorm(
    input: &Path,
    index: usize,
    cancel: &CancellationToken,
    mut on_time: impl FnMut(Duration),
) -> Result<String> {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-progress",
            "pipe:1",
        ])
        .arg("-i")
        .arg(input)
        .args(["-map", &format!("0:{index}")])
        .args(["-af", "loudnorm=print_format=json", "-f", "null", "-"]);
    process::stream_lines(command, "ffmpeg", cancel, |line| {
        if let Some(time) = parse_out_time(line) {
            on_time(time);
        }
        Ok(())
    })
    .await
}

/// Parses a `-progress` line such as `out_time_us=10100000`. Returns `None` for the other keys,
/// and for `N/A` before the first frame.
fn parse_out_time(line: &str) -> Option<Duration> {
    let micros = line.strip_prefix("out_time_us=")?.parse().ok()?;
    Some(Duration::from_micros(micros))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_progress_times() {
        assert_eq!(
            parse_out_time("out_time_us=10100000"),
            Some(Duration::from_millis(10_100))
        );
        assert_eq!(parse_out_time("out_time_us=N/A"), None);
        assert_eq!(parse_out_time("out_time=00:00:10.100000"), None);
    }
}
