//! The stretches of a file that the models separate, taken from its transcript.
//!
//! Each line of the transcript is padded, so the model sees the breath before and the tail after
//! the words, and lines that sit close together are merged, so the model isn't restarted for a
//! pause. Everything outside the spans passes through untouched.

use std::time::Duration;

use crate::srt::Block;

/// Added either side of a line. The stage's fade stays within it, so the speech itself is never
/// faded (`FADE_S` in `ml/src/vt_ml/separate/pipeline.py`).
const PAD: Duration = Duration::from_millis(250);

/// Lines with less than this between them become one span.
const MERGE_GAP: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: Duration,
    pub end: Duration,
}

/// Padded, sorted, and merged spans for the transcript's lines. They aren't clamped to the file,
/// since the stage knows its length to the sample.
pub fn spans(blocks: &[Block]) -> Vec<Span> {
    let mut padded: Vec<Span> = blocks
        .iter()
        .map(|block| Span {
            start: block.start.saturating_sub(PAD),
            end: block.end + PAD,
        })
        .collect();
    padded.sort_by_key(|span| span.start);

    let mut merged: Vec<Span> = Vec::with_capacity(padded.len());
    for span in padded {
        match merged.last_mut() {
            Some(last) if span.start <= last.end + MERGE_GAP => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// The spans as the JSON the stage reads: a list of `[start, end]` in seconds.
pub fn json(spans: &[Span]) -> String {
    let pairs: Vec<[f64; 2]> = spans
        .iter()
        .map(|s| [s.start.as_secs_f64(), s.end.as_secs_f64()])
        .collect();
    serde_json::to_string(&pairs).expect("floats serialize")
}

/// Total length of the spans within a file of length `total`.
pub fn duration(spans: &[Span], total: Option<Duration>) -> Duration {
    spans
        .iter()
        .map(|s| {
            let end = total.map_or(s.end, |t| s.end.min(t));
            end.saturating_sub(s.start)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(start_ms: u64, end_ms: u64) -> Block {
        Block {
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(end_ms),
            text: String::new(),
        }
    }

    fn span(start_ms: u64, end_ms: u64) -> Span {
        Span {
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(end_ms),
        }
    }

    #[test]
    fn pads_each_line() {
        assert_eq!(spans(&[block(5000, 6000)]), [span(4750, 6250)]);
    }

    #[test]
    fn clamps_to_the_start_of_the_file() {
        assert_eq!(spans(&[block(100, 900)]), [span(0, 1150)]);
    }

    #[test]
    fn merges_lines_less_than_a_second_apart() {
        // The padded spans end at 3250 and start at 3750, 500 ms apart.
        assert_eq!(
            spans(&[block(1000, 3000), block(4000, 5000)]),
            [span(750, 5250)]
        );
    }

    #[test]
    fn keeps_lines_further_apart_separate() {
        // The padded spans end at 2250 and start at 4750, 2.5 s apart.
        assert_eq!(
            spans(&[block(1000, 2000), block(5000, 6000)]),
            [span(750, 2250), span(4750, 6250)]
        );
    }

    #[test]
    fn sorts_and_merges_overlapping_lines() {
        assert_eq!(
            spans(&[block(7000, 9000), block(2000, 6000), block(3000, 4000)],),
            [span(1750, 9250)]
        );
    }

    #[test]
    fn writes_seconds_as_json() {
        assert_eq!(
            json(&[span(750, 2250), span(4750, 6250)]),
            "[[0.75,2.25],[4.75,6.25]]"
        );
    }

    #[test]
    fn totals_the_span_lengths_within_the_file() {
        let spans = [span(750, 2250), span(4750, 6250), span(20_000, 21_000)];

        assert_eq!(duration(&spans, None), Duration::from_secs(4));
        assert_eq!(
            duration(&spans, Some(Duration::from_secs(5))),
            Duration::from_millis(1500 + 250)
        );
    }
}
