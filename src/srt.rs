//! Reads and writes SRT subtitles.
//!
//! Transcripts are meant to be edited by hand before other tools read them, so parsing accepts
//! blocks that are unnumbered, out of order, overlapping, or without text, and `.` as well as
//! `,` before the milliseconds.

use std::{fmt::Write, time::Duration};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub start: Duration,
    pub end: Duration,
    pub text: String,
}

pub fn parse(srt: &str) -> Result<Vec<Block>> {
    let srt = srt
        .strip_prefix('\u{feff}')
        .unwrap_or(srt)
        .replace("\r\n", "\n");
    srt.split("\n\n")
        .map(str::trim)
        .filter(|chunk| !chunk.is_empty())
        .enumerate()
        .map(|(i, chunk)| parse_block(chunk).with_context(|| format!("block {}", i + 1)))
        .collect()
}

fn parse_block(chunk: &str) -> Result<Block> {
    let mut lines = chunk.lines().peekable();
    if lines
        .peek()
        .is_some_and(|line| line.trim().chars().all(|c| c.is_ascii_digit()))
    {
        lines.next();
    }
    let timing = lines.next().context("missing its start --> end line")?;
    let (start, end) = timing
        .split_once("-->")
        .with_context(|| format!("expected start --> end, not {timing:?}"))?;
    let (start, end) = (timestamp(start.trim())?, timestamp(end.trim())?);
    if end <= start {
        bail!(
            "ends at {} before it starts at {}",
            format_time(end),
            format_time(start)
        );
    }
    Ok(Block {
        start,
        end,
        text: lines.collect::<Vec<_>>().join("\n"),
    })
}

/// `HH:MM:SS,mmm`, with `.` also accepted before the milliseconds.
fn timestamp(text: &str) -> Result<Duration> {
    let bad = || format!("bad timestamp {text:?}; expected HH:MM:SS,mmm");
    let (clock, millis) = text.split_once([',', '.']).with_context(bad)?;
    let parts: Vec<&str> = clock.split(':').collect();
    let [hours, minutes, seconds] = parts[..] else {
        bail!(bad());
    };
    let number = |part: &str| -> Result<u64> {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            bail!(bad());
        }
        part.parse().with_context(bad)
    };
    let (minutes, seconds) = (number(minutes)?, number(seconds)?);
    if minutes > 59 || seconds > 59 || millis.len() != 3 {
        bail!(bad());
    }
    Ok(
        Duration::from_secs(number(hours)? * 3600 + minutes * 60 + seconds)
            + Duration::from_millis(number(millis)?),
    )
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

    #[test]
    fn parses_what_it_formats() {
        let blocks = vec![
            block(0, 1500, "First"),
            block(62_000, 64_900, "That was\nsick!"),
        ];

        assert_eq!(parse(&format(&blocks)).unwrap(), blocks);
    }

    #[test]
    fn accepts_hand_edits() {
        let srt = "\u{feff}7\r\n00:00:10.000 --> 00:00:12,500\r\nOut of order\r\n\r\n\
                   00:00:01,000 --> 00:00:02,000\r\n\r\n\r\n\
                   3\r\n00:00:01,500 --> 00:00:03,000\r\nmumbling\r\n";

        assert_eq!(
            parse(srt).unwrap(),
            [
                block(10_000, 12_500, "Out of order"),
                block(1000, 2000, ""),
                block(1500, 3000, "mumbling"),
            ]
        );
    }

    #[test]
    fn parses_an_empty_transcript() {
        assert_eq!(parse("").unwrap(), []);
        assert_eq!(parse("\n\n").unwrap(), []);
    }

    fn error(srt: &str) -> String {
        format!("{:#}", parse(srt).unwrap_err())
    }

    #[test]
    fn names_the_block_with_a_bad_timestamp() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nfine\n\n2\n00:00:61,000 --> 00:01:02,000\n";

        assert_eq!(
            error(srt),
            r#"block 2: bad timestamp "00:00:61,000"; expected HH:MM:SS,mmm"#
        );
    }

    #[test]
    fn rejects_a_block_without_timing() {
        assert_eq!(
            error("1\nhello\n"),
            r#"block 1: expected start --> end, not "hello""#
        );
    }

    #[test]
    fn rejects_a_block_that_ends_before_it_starts() {
        assert_eq!(
            error("00:00:05,000 --> 00:00:04,000\n"),
            "block 1: ends at 00:00:04,000 before it starts at 00:00:05,000"
        );
    }

    #[test]
    fn rejects_malformed_timestamps() {
        for bad in [
            "0:00:01",
            "00:00:01,5",
            "00:x0:01,000",
            "00:00,000",
            "-1:00:01,000",
        ] {
            let srt = format!("{bad} --> 00:00:09,000\n");
            assert!(parse(&srt).is_err(), "accepted {bad:?}");
        }
    }
}
