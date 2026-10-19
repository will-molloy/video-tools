pub mod chapters;
pub mod sync_check;

/// Tools the TUI offers for a picked file. Each maps to a CLI subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Chapters,
    SyncCheck,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::Chapters, Tool::SyncCheck];

    pub fn name(self) -> &'static str {
        match self {
            Tool::Chapters => "Chapters",
            Tool::SyncCheck => "Sync Check",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Tool::Chapters => "YouTube description chapter list",
            Tool::SyncCheck => "Diagnose audio/video sync problems",
        }
    }
}
