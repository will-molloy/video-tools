use std::{
    fmt,
    path::{Path, PathBuf},
};

use anyhow::anyhow;

pub mod chapters;
pub mod sync_check;
pub mod transcribe;

/// An output a tool won't replace unless forced. The CLI forces with `--force`, and the TUI
/// asks first.
#[derive(Debug)]
pub struct Exists(pub PathBuf);

impl fmt::Display for Exists {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.0.file_name().unwrap_or(self.0.as_os_str());
        write!(f, "{} already exists", name.to_string_lossy())
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

/// Tools the TUI offers for a picked file. Each maps to a CLI subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Chapters,
    SyncCheck,
    Transcribe,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::Chapters, Tool::SyncCheck, Tool::Transcribe];

    pub fn name(self) -> &'static str {
        match self {
            Tool::Chapters => "Chapters",
            Tool::SyncCheck => "Sync Check",
            Tool::Transcribe => "Transcribe",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Tool::Chapters => "YouTube description chapter list",
            Tool::SyncCheck => "Diagnose audio/video sync problems",
            Tool::Transcribe => "Extract dialogue to .srt file",
        }
    }
}
