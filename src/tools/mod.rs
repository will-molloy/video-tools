use std::{
    fmt,
    num::NonZero,
    path::{Path, PathBuf},
};

use anyhow::anyhow;

use crate::display;

pub mod chapters;
pub mod cli;
pub mod dialogue_separation;
pub mod loudness_check;
pub mod sync_check;
pub mod transcribe;

/// An output a tool won't replace unless forced. The CLI forces with `--force`, and the TUI
/// asks first.
#[derive(Debug)]
pub struct Exists(pub PathBuf);

impl fmt::Display for Exists {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} already exists", display::name(&self.0))
    }
}

impl std::error::Error for Exists {}

/// Fails with [`Exists`] if `output` exists and `force` is unset.
pub fn check_output(output: &Path, force: bool) -> Result<(), Exists> {
    if !force && output.exists() {
        return Err(Exists(output.to_owned()));
    }
    Ok(())
}

/// Tells the CLI user how to get past an [`Exists`] error.
pub fn suggest_force(e: anyhow::Error) -> anyhow::Error {
    match e.downcast::<Exists>() {
        Ok(exists) => anyhow!("{exists}; pass --force to replace it"),
        Err(e) => e,
    }
}

/// Tools the TUI offers for the picked files. Each maps to a CLI subcommand, and `summary` is also
/// its CLI help and its README description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Chapters,
    SyncCheck,
    LoudnessCheck,
    Transcribe,
    DialogueSeparation,
}

impl Tool {
    pub const ALL: [Tool; 5] = [
        Tool::Chapters,
        Tool::SyncCheck,
        Tool::LoudnessCheck,
        Tool::Transcribe,
        Tool::DialogueSeparation,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Tool::Chapters => "Chapters",
            Tool::SyncCheck => "Sync Check",
            Tool::LoudnessCheck => "Loudness Check",
            Tool::Transcribe => "Transcribe",
            Tool::DialogueSeparation => "Dialogue Separation",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Tool::Chapters => "YouTube description chapter list",
            Tool::SyncCheck => "Diagnose audio/video sync problems",
            Tool::LoudnessCheck => "Check YouTube loudness targets",
            Tool::Transcribe => "Extract dialogue to .srt file",
            Tool::DialogueSeparation => "Remove background noise or dialogue",
        }
    }

    /// How many files the tool runs on at once.
    pub fn max_parallel(self) -> usize {
        match self {
            // Each run is one ffprobe or ffmpeg process on one core, so the limit is the number of
            // CPU threads.
            Tool::Chapters | Tool::SyncCheck | Tool::LoudnessCheck => {
                std::thread::available_parallelism().map_or(1, NonZero::get)
            }

            // Each run needs the GPU to itself.
            Tool::Transcribe | Tool::DialogueSeparation => 1,
        }
    }
}
