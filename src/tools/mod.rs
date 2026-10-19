pub mod chapters;

/// Tools the TUI offers for a picked file. Each maps to a CLI subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {}

impl Tool {
    pub const ALL: [Tool; 0] = [];

    pub fn name(self) -> &'static str {
        match self {}
    }

    pub fn summary(self) -> &'static str {
        match self {}
    }
}
