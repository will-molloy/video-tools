//! Writes the kept stem after separation.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;

use crate::ffmpeg::{self, AudioEncoding};

/// How to write the kept stem.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// A float WAV at the source's sample rate, for audio-only inputs.
    Wav { path: PathBuf, sample_rate: u32 },

    /// A copy of the input with its audio replaced, for inputs with video.
    Remux {
        path: PathBuf,
        audio: AudioStream,
        encoding: AudioEncoding,
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

/// Writes `stem` as `output`.
pub async fn write(input: &Path, stem: &Path, output: &Output) -> Result<()> {
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
            args.extend(encoding.args("a"));
            args.extend(["-ar".into(), audio.sample_rate.to_string().into()]);
        }
    }
    ffmpeg::write(args, output.path(), |_| {}).await
}
