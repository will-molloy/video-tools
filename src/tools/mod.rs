pub mod chapters;

/// Tools the TUI offers for a picked file. Each maps to a CLI subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Chapters,
}

impl Tool {
    pub const ALL: [Tool; 1] = [Tool::Chapters];

    pub fn name(self) -> &'static str {
        match self {
            Tool::Chapters => "Chapters",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Tool::Chapters => "YouTube description chapter list",
        }
    }
}
