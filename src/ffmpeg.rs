//! Runs ffmpeg.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};

use crate::{
    display,
    ffprobe::{self, Stream, StreamKind},
};

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

/// Runs ffmpeg quietly, overwriting outputs. `what` names the step in the error.
pub async fn run(args: impl IntoIterator<Item = OsString>, what: &str) -> Result<()> {
    let output = command()
        .args(args)
        .stdin(Stdio::null())
        // Cancelling the future kills ffmpeg.
        .kill_on_drop(true)
        .output()
        .await
        .context(NOT_FOUND)?;
    if !output.status.success() {
        bail!(
            "ffmpeg failed to {what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Runs ffmpeg with `args` to write `output`, via a `.partial` file so a failed or cancelled
/// write leaves no half-written output behind. Reports how far into the timeline ffmpeg has
/// written as it goes.
pub async fn write(
    args: impl IntoIterator<Item = OsString>,
    output: &Path,
    mut on_progress: impl FnMut(Duration),
) -> Result<()> {
    let partial = Partial(partial_path(output));
    let mut child = command()
        .args(["-progress", "pipe:1", "-nostats"])
        .args(args)
        .arg(&partial.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Cancelling the future kills ffmpeg.
        .kill_on_drop(true)
        .spawn()
        .context(NOT_FOUND)?;

    // Drained concurrently, because a pipe nobody reads fills up and blocks ffmpeg mid-write.
    let mut stderr = child.stderr.take().context("ffmpeg stderr not captured")?;
    let stderr = async move {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await.map(|_| buf)
    };
    let stdout = child.stdout.take().context("ffmpeg stdout not captured")?;
    let progress = async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            if let Some(time) = progress_time(&line) {
                on_progress(time);
            }
        }
        std::io::Result::Ok(())
    };
    let (stderr, progress) = tokio::join!(stderr, progress);
    progress.context("failed to read ffmpeg progress")?;
    let stderr = stderr.context("failed to read ffmpeg stderr")?;

    let status = child.wait().await.context("failed to wait for ffmpeg")?;
    if !status.success() {
        bail!(
            "ffmpeg failed to write {}: {}",
            display::name(output),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    fs::rename(&partial.0, output)
        .with_context(|| format!("failed to write {}", display::path(output)))?;
    Ok(())
}

const NOT_FOUND: &str = "failed to run ffmpeg; is it installed and on PATH?";

fn command() -> Command {
    let mut command = Command::new("ffmpeg");
    command.args(["-nostdin", "-v", "error", "-y"]);
    command
}

/// The time in a `-progress` line such as `out_time_us=1500000`. ffmpeg reports `N/A` before
/// the first frame.
fn progress_time(line: &str) -> Option<Duration> {
    let micros = line.strip_prefix("out_time_us=")?.parse().ok()?;
    Some(Duration::from_micros(micros))
}

/// `talk.voice.mp4` becomes `talk.voice.partial.mp4`, keeping the extension ffmpeg infers the
/// container from.
fn partial_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = match path.extension() {
        Some(ext) => format!("{stem}.partial.{}", ext.to_string_lossy()),
        None => format!("{stem}.partial"),
    };
    path.with_file_name(name)
}

/// Deletes the file on drop unless it was renamed away.
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The encoder that matches an audio stream's codec and bit rate.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioEncoding {
    pub encoder: String,

    /// `None` for lossless codecs.
    pub bit_rate: Option<u64>,
}

impl AudioEncoding {
    /// Picks the encoder for `stream`, measuring the bit rate when the container doesn't record
    /// it, as in Matroska files ffmpeg wrote.
    pub async fn for_stream(input: &Path, stream: &Stream) -> Result<AudioEncoding> {
        let StreamKind::Audio { bit_rate, .. } = stream.kind else {
            bail!("stream {} is not audio", stream.index);
        };
        let encoder = audio_encoder(&stream.codec).to_string();
        if is_lossless(&stream.codec) {
            return Ok(AudioEncoding {
                encoder,
                bit_rate: None,
            });
        }
        let bit_rate = match bit_rate {
            Some(rate) => rate,
            None => ffprobe::measured_bit_rate(input, stream.index)
                .await?
                .with_context(|| format!("can't tell audio #{}'s bit rate", stream.index))?,
        };
        Ok(AudioEncoding {
            encoder,
            bit_rate: Some(bit_rate),
        })
    }

    /// The arguments that encode output stream `spec`, such as `a:0`, this way.
    pub fn args(&self, spec: &str) -> Vec<OsString> {
        let mut args: Vec<OsString> =
            vec![format!("-c:{spec}").into(), self.encoder.clone().into()];
        if let Some(rate) = self.bit_rate {
            args.extend([format!("-b:{spec}").into(), rate.to_string().into()]);
        }
        args
    }
}

/// ffmpeg's native encoders, where they're the best choice, share their codec's name.
fn audio_encoder(codec: &str) -> &str {
    match codec {
        "mp3" => "libmp3lame",
        "opus" => "libopus",
        "vorbis" => "libvorbis",
        codec => codec,
    }
}

fn is_lossless(codec: &str) -> bool {
    matches!(codec, "alac" | "flac" | "mlp" | "truehd" | "wavpack") || codec.starts_with("pcm_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_codecs_to_encoders() {
        assert_eq!(audio_encoder("aac"), "aac");
        assert_eq!(audio_encoder("opus"), "libopus");
        assert_eq!(audio_encoder("mp3"), "libmp3lame");
    }

    #[test]
    fn treats_pcm_and_flac_as_lossless() {
        assert!(is_lossless("pcm_s24le"));
        assert!(is_lossless("flac"));
        assert!(!is_lossless("aac"));
    }

    #[test]
    fn names_partial_files_before_the_extension() {
        assert_eq!(
            partial_path(Path::new("/v/talk.voice.mp4")),
            Path::new("/v/talk.voice.partial.mp4")
        );
    }

    #[test]
    fn reads_the_time_from_progress_lines() {
        assert_eq!(
            progress_time("out_time_us=1500000"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(progress_time("out_time_us=N/A"), None);
        assert_eq!(progress_time("frame=12"), None);
    }

    #[test]
    fn encodes_lossy_audio_at_its_bit_rate() {
        let encoding = AudioEncoding {
            encoder: "aac".into(),
            bit_rate: Some(64_000),
        };

        assert_eq!(encoding.args("a:1"), ["-c:a:1", "aac", "-b:a:1", "64000"]);
    }
}
