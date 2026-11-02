//! Writes SRT subtitles.

use std::{fmt::Write, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub start: Duration,
    pub end: Duration,
    pub text: String,
}

/// Numbers the blocks from 1 in the order given.
pub fn format(blocks: &[Block]) -> String {
    let mut srt = String::new();
    for (i, block) in blocks.iter().enumerate() {
        let _ = write!(
            srt,
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            format_time(block.start),
            format_time(block.end),
            block.text
        );
    }
    srt
}

fn format_time(time: Duration) -> String {
    let millis = time.as_millis();
    format!(
        "{:02}:{:02}:{:02},{:03}",
        millis / 3_600_000,
        millis / 60_000 % 60,
        millis / 1000 % 60,
        millis % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(start_ms: u64, end_ms: u64, text: &str) -> Block {
        Block {
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(end_ms),
            text: text.into(),
        }
    }

    #[test]
    fn formats_numbered_blocks() {
        let blocks = [
            block(4120, 7860, "Okay, we're at the top."),
            block(3_723_004, 3_725_000, "Two lines\nof text"),
        ];

        assert_eq!(
            format(&blocks),
            "1\n00:00:04,120 --> 00:00:07,860\nOkay, we're at the top.\n\n\
             2\n01:02:03,004 --> 01:02:05,000\nTwo lines\nof text\n\n"
        );
    }

    #[test]
    fn formats_no_blocks_as_an_empty_file() {
        assert_eq!(format(&[]), "");
    }
}
