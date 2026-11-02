//! `vt chapters`: print container chapters as a YouTube description chapter list.

use std::{fmt, iter, path::Path, path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::Args;

use crate::{
    ffprobe,
    quote::{plain, quote},
};

/// YouTube ignores chapters shorter than this.
const MIN_CHAPTER_SECS: u64 = 10;

/// YouTube needs at least this many chapters to show any.
const MIN_CHAPTERS: usize = 3;

#[derive(Debug, Args)]
pub struct ChaptersArgs {
    /// Video file to read chapters from
    file: PathBuf,
}

pub async fn cli(args: &ChaptersArgs) -> Result<ExitCode> {
    match run(&args.file).await? {
        Outcome::NoChapters => {
            eprintln!("{} has no chapters", plain(&quote(&args.file)));
            Ok(ExitCode::FAILURE)
        }
        Outcome::Chapters(report) => {
            // Chapters to stdout and warnings to stderr, so `vt chapters f.mkv | wl-copy` copies
            // just the list.
            for line in report.lines() {
                println!("{line}");
            }
            for violation in &report.violations {
                eprintln!("warning: {violation}");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub async fn run(path: &Path) -> Result<Outcome> {
    Ok(Outcome::from_probe(ffprobe::chapters(path).await?))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    NoChapters,
    Chapters(Report),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub style: TimestampStyle,
    pub entries: Vec<Entry>,
    pub violations: Vec<Violation>,
}

/// One line of the chapter list, at the whole-second resolution YouTube sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub start_secs: u64,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampStyle {
    /// `M:SS` or `MM:SS`, for videos under an hour.
    Minutes,
    /// `H:MM:SS`.
    Hours,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    FirstNotAtZero {
        start_secs: u64,
    },
    TooFew {
        count: usize,
    },
    TooShort {
        number: usize,
        title: String,
        length_secs: u64,
    },
    NotAscending {
        number: usize,
        title: String,
    },
}

impl Outcome {
    fn from_probe(probe: ffprobe::Chapters) -> Self {
        let Some(last) = probe.chapters.last() else {
            return Outcome::NoChapters;
        };
        let end_secs = probe.duration.unwrap_or(last.end).as_secs();

        // Keep container order rather than sorting, so validation can flag misordered chapters.
        let entries: Vec<Entry> = probe
            .chapters
            .into_iter()
            .enumerate()
            .map(|(i, chapter)| Entry {
                start_secs: chapter.start.as_secs(),
                title: chapter
                    .title
                    .unwrap_or_else(|| format!("Chapter {}", i + 1)),
            })
            .collect();

        Outcome::Chapters(Report {
            style: TimestampStyle::for_video(end_secs),
            violations: validate(&entries, end_secs),
            entries,
        })
    }
}

impl Report {
    pub fn lines(&self) -> impl Iterator<Item = String> + '_ {
        self.entries
            .iter()
            .map(|e| format!("{} {}", self.style.format(e.start_secs), e.title))
    }
}

impl TimestampStyle {
    pub fn for_video(length_secs: u64) -> Self {
        if length_secs < 3600 {
            TimestampStyle::Minutes
        } else {
            TimestampStyle::Hours
        }
    }

    pub fn format(self, secs: u64) -> String {
        let (minutes, seconds) = (secs / 60, secs % 60);
        match self {
            TimestampStyle::Minutes => format!("{minutes}:{seconds:02}"),
            TimestampStyle::Hours => format!("{}:{:02}:{seconds:02}", minutes / 60, minutes % 60),
        }
    }
}

/// Checks the list against YouTube's chapter rules. `end_secs` is the video length.
pub fn validate(entries: &[Entry], end_secs: u64) -> Vec<Violation> {
    let mut violations = Vec::new();

    if let Some(first) = entries.first()
        && first.start_secs != 0
    {
        violations.push(Violation::FirstNotAtZero {
            start_secs: first.start_secs,
        });
    }

    if entries.len() < MIN_CHAPTERS {
        violations.push(Violation::TooFew {
            count: entries.len(),
        });
    }

    for (i, pair) in entries.windows(2).enumerate() {
        if pair[1].start_secs <= pair[0].start_secs {
            violations.push(Violation::NotAscending {
                number: i + 2,
                title: pair[1].title.clone(),
            });
        }
    }

    // Each chapter runs until the next one starts, and the last until the end of the video.
    let ends = entries
        .iter()
        .skip(1)
        .map(|e| e.start_secs)
        .chain(iter::once(end_secs));
    for (i, (entry, end)) in entries.iter().zip(ends).enumerate() {
        // Skips misordered pairs, which NotAscending already covers.
        let Some(length_secs) = end.checked_sub(entry.start_secs).filter(|&len| len > 0) else {
            continue;
        };
        if length_secs < MIN_CHAPTER_SECS {
            violations.push(Violation::TooShort {
                number: i + 1,
                title: entry.title.clone(),
                length_secs,
            });
        }
    }

    violations
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::FirstNotAtZero { start_secs } => write!(
                f,
                "first chapter starts at {start_secs}s; YouTube requires 0:00"
            ),
            Violation::TooFew { count } => write!(
                f,
                "only {count} chapter(s); YouTube requires at least {MIN_CHAPTERS}"
            ),
            Violation::TooShort {
                number,
                title,
                length_secs,
            } => write!(
                f,
                "chapter {number} {title:?} is {length_secs}s long; \
                 YouTube requires at least {MIN_CHAPTER_SECS}s"
            ),
            Violation::NotAscending { number, title } => write!(
                f,
                "chapter {number} {title:?} does not start after the one before it"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(starts: &[u64]) -> Vec<Entry> {
        starts
            .iter()
            .enumerate()
            .map(|(i, &start_secs)| Entry {
                start_secs,
                title: format!("Part {}", i + 1),
            })
            .collect()
    }

    #[test]
    fn style_switches_to_hours_at_one_hour() {
        assert_eq!(TimestampStyle::for_video(3599), TimestampStyle::Minutes);
        assert_eq!(TimestampStyle::for_video(3600), TimestampStyle::Hours);
    }

    #[test]
    fn formats_minutes_style() {
        let format = |secs| TimestampStyle::Minutes.format(secs);

        assert_eq!(format(0), "0:00");
        assert_eq!(format(7), "0:07");
        assert_eq!(format(65), "1:05");
        assert_eq!(format(754), "12:34");
        assert_eq!(format(3599), "59:59");
    }

    #[test]
    fn formats_hours_style() {
        let format = |secs| TimestampStyle::Hours.format(secs);

        assert_eq!(format(0), "0:00:00");
        assert_eq!(format(65), "0:01:05");
        assert_eq!(format(3600), "1:00:00");
        assert_eq!(format(36_000 + 754), "10:12:34");
    }

    #[test]
    fn valid_list_has_no_violations() {
        assert_eq!(validate(&entries(&[0, 10, 20]), 30), vec![]);
    }

    #[test]
    fn flags_first_chapter_not_at_zero() {
        assert_eq!(
            validate(&entries(&[5, 15, 25]), 35),
            vec![Violation::FirstNotAtZero { start_secs: 5 }]
        );
    }

    #[test]
    fn flags_fewer_than_three_chapters() {
        assert_eq!(
            validate(&entries(&[0, 10]), 20),
            vec![Violation::TooFew { count: 2 }]
        );
    }

    #[test]
    fn flags_short_chapters_including_the_last() {
        assert_eq!(
            validate(&entries(&[0, 9, 20]), 29),
            vec![
                Violation::TooShort {
                    number: 1,
                    title: "Part 1".into(),
                    length_secs: 9
                },
                Violation::TooShort {
                    number: 3,
                    title: "Part 3".into(),
                    length_secs: 9
                },
            ]
        );
    }

    #[test]
    fn flags_out_of_order_and_duplicate_starts() {
        assert_eq!(
            validate(&entries(&[0, 30, 20, 20, 40]), 50),
            vec![
                Violation::NotAscending {
                    number: 3,
                    title: "Part 3".into()
                },
                Violation::NotAscending {
                    number: 4,
                    title: "Part 4".into()
                },
            ]
        );
    }

    #[test]
    fn builds_report_from_probe() {
        let chapter = |start_ms, end_ms, title: Option<&str>| ffprobe::Chapter {
            start: std::time::Duration::from_millis(start_ms),
            end: std::time::Duration::from_millis(end_ms),
            title: title.map(Into::into),
        };
        let probe = ffprobe::Chapters {
            chapters: vec![
                chapter(400, 30_500, Some("Intro")),
                chapter(30_500, 65_000, Some("Setup")),
                chapter(65_000, 120_000, None),
            ],
            duration: Some(std::time::Duration::from_millis(120_000)),
        };

        let Outcome::Chapters(report) = Outcome::from_probe(probe) else {
            panic!("expected chapters");
        };

        assert_eq!(
            report.lines().collect::<Vec<_>>(),
            ["0:00 Intro", "0:30 Setup", "1:05 Chapter 3"]
        );
        assert_eq!(report.violations, vec![]);
    }

    #[test]
    fn reports_no_chapters() {
        let probe = ffprobe::Chapters {
            chapters: vec![],
            duration: Some(std::time::Duration::from_secs(60)),
        };

        assert_eq!(Outcome::from_probe(probe), Outcome::NoChapters);
    }
}
