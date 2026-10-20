//! Writes the kept stem after separation.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::{
    ffmpeg,
    ffprobe::{self, Stream, StreamKind},
};

/// How to write the kept stem.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// A float WAV at the source's sample rate, for audio-only inputs.
    Wav { path: PathBuf, sample_rate: u32 },

    /// A copy of the input with its audio replaced, for inputs with video.
    Remux {
        path: PathBuf,
        audio: AudioStream,
        encoding: Encoding,
    },
}

impl Output {
    pub fn path(&self) -> &Path {
        match self {
            Output::Wav { path, .. } | Output::Remux { path, .. } => path,
        }
    }
}

/// The source audio stream a remux replaces.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioStream {
    pub index: usize,
    pub sample_rate: u32,

    /// Applied to the new stream so it stays in sync with the copied video.
    pub start: Duration,
}

/// The encoder that matches the source audio's codec and bit rate.
#[derive(Debug, Clone, PartialEq)]
pub struct Encoding {
    pub encoder: String,

    /// `None` for lossless codecs.
    pub bit_rate: Option<u64>,
}

impl Encoding {
    /// Picks the encoder for `codec`, measuring the bit rate when the container doesn't record
    /// it, as in Matroska files ffmpeg wrote.
    pub async fn for_stream(input: &Path, stream: &Stream) -> Result<Encoding> {
        let StreamKind::Audio { bit_rate, .. } = stream.kind else {
            bail!("stream {} is not audio", stream.index);
        };
        let encoder = encoder(&stream.codec).to_string();
        if is_lossless(&stream.codec) {
            return Ok(Encoding {
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
        Ok(Encoding {
            encoder,
            bit_rate: Some(bit_rate),
        })
    }
}

/// ffmpeg's native encoders, where they're the best choice, share their codec's name.
fn encoder(codec: &str) -> &str {
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

/// Writes `stem` as `output`, via a `.partial` file so a failed or cancelled write leaves no
/// half-written output behind.
pub async fn write(input: &Path, stem: &Path, output: &Output) -> Result<()> {
    let path = output.path();
    let partial = Partial(partial_path(path));
    let mut args: Vec<OsString> = Vec::new();
    match output {
        Output::Wav { sample_rate, .. } => {
            args.extend(["-i".into(), stem.into()]);
            args.extend(["-ar".into(), sample_rate.to_string().into()]);
            args.extend([
                "-c:a".into(),
                "pcm_f32le".into(),
                "-rf64".into(),
                "auto".into(),
            ]);
        }
        Output::Remux {
            audio, encoding, ..
        } => {
            args.extend(["-i".into(), input.into()]);
            args.extend([
                "-itsoffset".into(),
                format!("{:.6}", audio.start.as_secs_f64()).into(),
            ]);
            args.extend(["-i".into(), stem.into()]);
            // `V` skips cover art, which ffprobe models as a one-frame video stream.
            for map in ["0:V", "1:a", "0:s?", "0:t?"] {
                args.extend(["-map".into(), map.into()]);
            }
            args.extend([
                "-map_metadata".into(),
                "0".into(),
                "-map_chapters".into(),
                "0".into(),
            ]);
            // Keeps the original audio stream's language and title.
            args.extend([
                "-map_metadata:s:a:0".into(),
                format!("0:s:{}", audio.index).into(),
            ]);
            args.extend(["-c".into(), "copy".into()]);
            args.extend(["-c:a".into(), encoding.encoder.clone().into()]);
            if let Some(rate) = encoding.bit_rate {
                args.extend(["-b:a".into(), rate.to_string().into()]);
            }
            args.extend(["-ar".into(), audio.sample_rate.to_string().into()]);
        }
    }
    args.push(partial.0.clone().into());
    ffmpeg::run(args, "write the output").await?;
    fs::rename(&partial.0, path).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_codecs_to_encoders() {
        assert_eq!(encoder("aac"), "aac");
        assert_eq!(encoder("opus"), "libopus");
        assert_eq!(encoder("mp3"), "libmp3lame");
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
}
