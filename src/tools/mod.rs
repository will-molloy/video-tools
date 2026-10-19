pub mod chapters;
pub mod sync_check;
pub mod transcribe;

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
            Tool::Transcribe => "Editable subtitles of the speech",
        }
    }
}
