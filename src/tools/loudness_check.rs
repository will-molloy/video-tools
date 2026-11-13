//! `vt loudness-check`: measure a file's loudness and check it against YouTube's targets.
//!
//! ffmpeg's `loudnorm` filter measures the first audio stream of the whole file. The tool only
//! reports, and never changes the file.

use std::{
    fmt,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{
    ffmpeg,
    ffprobe::{self, StreamKind},
    tools::{self, Tool, cli::Printed, transcribe::clock},
};

/// YouTube's integrated loudness target, in LUFS.
const TARGET: Tenths = Tenths(-140);

/// How far integrated loudness may stray from [`TARGET`], in LU.
const TOLERANCE: Tenths = Tenths(10);

/// The highest true peak YouTube allows, in dBTP.
const MAX_TRUE_PEAK: Tenths = Tenths(-10);

#[derive(Debug, Args)]
pub struct LoudnessCheckArgs {
    /// Video or audio files to measure
    #[arg(required = true)]
    files: Vec<PathBuf>,
}

/// Flags a file when it fails a check.
pub async fn cli(args: &LoudnessCheckArgs) -> Result<ExitCode> {
    let cancel = CancellationToken::new();
    tools::cli::run(Tool::LoudnessCheck, &args.files, &cancel, |file| {
        let cancel = &cancel;
        async move {
            let report = run(file, cancel, |progress| {
                tools::cli::show_progress(file, &progress);
            })
            .await?;
            Ok(Printed {
                stdout: report.rows().into_iter().map(|row| row.text).collect(),
                stderr: vec![],
                flagged: report.flagged(),
            })
        }
    })
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub done: Duration,

    /// `None` when ffprobe can't tell the duration.
    pub total: Option<Duration>,
}

impl Progress {
    pub fn ratio(self) -> Option<f64> {
        let total = self.total.filter(|total| !total.is_zero())?;
        Some((self.done.as_secs_f64() / total.as_secs_f64()).min(1.0))
    }
}

impl fmt::Display for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.ratio(), self.total) {
            (Some(ratio), Some(total)) => write!(
                f,
                "Measuring… {:.0}% · {} of {}",
                ratio * 100.0,
                clock(self.done),
                clock(total)
            ),
            _ => write!(f, "Measuring… {}", clock(self.done)),
        }
    }
}

/// Measures the first audio stream of `path`.
pub async fn run(
    path: &Path,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(Progress),
) -> Result<Report> {
    let info = ffprobe::streams(path).await?;
    let audio = info
        .streams
        .iter()
        .find(|s| matches!(s.kind, StreamKind::Audio { .. }))
        .context("no audio stream")?;
    let total = info.duration;
    on_progress(Progress {
        done: Duration::ZERO,
        total,
    });
    let log = ffmpeg::loudnorm(path, audio.index, cancel, |done| {
        on_progress(Progress { done, total });
    })
    .await?;
    Report::parse(&log)
}

/// A level in tenths of a decibel, the precision loudness meters report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tenths(i32);

impl Tenths {
    /// Rounds `value` to the nearest tenth. Returns `None` for silence's `-inf`.
    #[allow(clippy::cast_possible_truncation)]
    fn round(value: f64) -> Option<Tenths> {
        let tenths = (value * 10.0).round();
        tenths.is_finite().then_some(Tenths(tenths as i32))
    }
}

/// `-16.6`, or `+2.6` with the `+` flag.
impl fmt::Display for Tenths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = match self.0 {
            ..0 => "-",
            0.. if f.sign_plus() => "+",
            0.. => "",
        };
        let abs = self.0.unsigned_abs();
        write!(f, "{sign}{}.{}", abs / 10, abs % 10)
    }
}

/// The checked measures of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// LUFS over the whole file.
    pub integrated: Tenths,

    /// dBTP.
    pub true_peak: Tenths,

    /// Loudness range, in LU.
    pub range: Tenths,
}

/// How a measure compares with its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
}

/// One measure of a [`Report`], as a line of text. `verdict` is `None` for a measure that's
/// reported but not checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub verdict: Option<Verdict>,
    pub text: String,
}

impl Report {
    /// Reads `loudnorm`'s JSON from the end of ffmpeg's log.
    fn parse(log: &str) -> Result<Report> {
        let json = loudnorm_json(log).context("no loudnorm measurements in ffmpeg's output")?;
        let raw: RawLoudnorm =
            serde_json::from_str(json).context("failed to parse loudnorm measurements")?;
        let level = |name: &str, text: &str| {
            let value: f64 = text
                .parse()
                .with_context(|| format!("loudnorm's {name} {text:?} isn't a number"))?;
            Tenths::round(value).with_context(|| format!("loudnorm's {name} is {text}"))
        };
        if raw.input_i == "-inf" {
            bail!("the audio is silent");
        }
        Ok(Report {
            integrated: level("input_i", &raw.input_i)?,
            true_peak: level("input_tp", &raw.input_tp)?,
            range: level("input_lra", &raw.input_lra)?,
        })
    }

    /// The change in dB that brings integrated loudness to the target.
    pub fn gain(&self) -> Tenths {
        Tenths(TARGET.0 - self.integrated.0)
    }

    pub fn integrated_verdict(&self) -> Verdict {
        if self.gain().0.abs() <= TOLERANCE.0 {
            Verdict::Pass
        } else {
            Verdict::Fail
        }
    }

    pub fn true_peak_verdict(&self) -> Verdict {
        if self.true_peak <= MAX_TRUE_PEAK {
            Verdict::Pass
        } else {
            Verdict::Fail
        }
    }

    /// Whether any check fails.
    pub fn flagged(&self) -> bool {
        [self.integrated_verdict(), self.true_peak_verdict()].contains(&Verdict::Fail)
    }

    /// Integrated loudness, true peak, then loudness range.
    pub fn rows(&self) -> [Row; 3] {
        let integrated = self.integrated_verdict();
        let target = format!("target {TARGET} ±{TOLERANCE} LUFS");
        let integrated_note = match (integrated, self.gain()) {
            (Verdict::Pass, _) => target,
            (Verdict::Fail, gain) if gain.0 > 0 => format!("{target}, raise by {gain:+} dB"),
            (Verdict::Fail, gain) => format!("{target}, lower by {gain:+} dB"),
        };
        let true_peak = self.true_peak_verdict();
        [
            row(
                "Integrated loudness",
                self.integrated,
                "LUFS",
                Some((integrated, &integrated_note)),
            ),
            row(
                "True peak",
                self.true_peak,
                "dBTP",
                Some((true_peak, &format!("max {MAX_TRUE_PEAK} dBTP"))),
            ),
            row("Loudness range", self.range, "LU", None),
        ]
    }
}

/// `Integrated loudness  -16.6 LUFS  FAIL  target -14.0 ±1.0 LUFS, raise by +2.6 dB`.
fn row(label: &str, value: Tenths, unit: &str, check: Option<(Verdict, &str)>) -> Row {
    let text = format!("{label:<19}  {:>6} {unit:<4}", value.to_string());
    match check {
        Some((verdict, note)) => {
            let word = match verdict {
                Verdict::Pass => "PASS",
                Verdict::Fail => "FAIL",
            };
            Row {
                verdict: Some(verdict),
                text: format!("{text}  {word}  {note}"),
            }
        }
        None => Row {
            verdict: None,
            text: text.trim_end().to_string(),
        },
    }
}

/// The JSON object `loudnorm` logs as ffmpeg finishes, between other log lines.
fn loudnorm_json(log: &str) -> Option<&str> {
    let after = &log[log.rfind("[Parsed_loudnorm_")?..];
    let start = after.find('{')?;
    let end = start + after[start..].find('}')?;
    Some(&after[start..=end])
}

/// `loudnorm` logs each measure as a string, such as `"-16.62"` or `"-inf"`.
#[derive(Deserialize)]
struct RawLoudnorm {
    input_i: String,
    input_tp: String,
    input_lra: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffmpeg 7.1's log from measuring `tests/fixtures/clean-cfr.mp4`.
    const LOG: &str = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'clean-cfr.mp4':
  Duration: 00:00:10.00, start: 0.000000, bitrate: 274 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 160x90 \
[SAR 1:1 DAR 16:9], 196 kb/s, 60 fps, 60 tbr, 90k tbn (default)
  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), 48000 Hz, mono, fltp, 64 kb/s \
(default)
Stream mapping:
  Stream #0:1 -> #0:0 (aac (native) -> pcm_s16le (native))
Output #0, null, to 'pipe:':
  Stream #0:0(und): Audio: pcm_s16le, 192000 Hz, mono, s16, 3072 kb/s (default)
[Parsed_loudnorm_0 @ 0x7f6cd4004a40]
{
\t\"input_i\" : \"-21.79\",
\t\"input_tp\" : \"-13.11\",
\t\"input_lra\" : \"0.00\",
\t\"input_thresh\" : \"-31.79\",
\t\"output_i\" : \"-24.01\",
\t\"output_tp\" : \"-15.31\",
\t\"output_lra\" : \"0.10\",
\t\"output_thresh\" : \"-34.01\",
\t\"normalization_type\" : \"dynamic\",
\t\"target_offset\" : \"0.01\"
}
[out#0/null @ 0x55e2403ddb00] video:0KiB audio:3752KiB subtitle:0KiB other streams:0KiB \
global headers:0KiB muxing overhead: unknown
size=N/A time=00:00:10.10 bitrate=N/A speed= 100x
";

    fn report(integrated: i32, true_peak: i32) -> Report {
        Report {
            integrated: Tenths(integrated),
            true_peak: Tenths(true_peak),
            range: Tenths(61),
        }
    }

    fn texts(report: &Report) -> Vec<String> {
        report.rows().into_iter().map(|row| row.text).collect()
    }

    #[test]
    fn parses_the_measurements_from_ffmpegs_log() {
        assert_eq!(
            Report::parse(LOG).unwrap(),
            Report {
                integrated: Tenths(-218),
                true_peak: Tenths(-131),
                range: Tenths(0),
            }
        );
    }

    #[test]
    fn fails_to_parse_a_silent_file() {
        let log = LOG.replace("\"-21.79\"", "\"-inf\"");

        assert_eq!(
            Report::parse(&log).unwrap_err().to_string(),
            "the audio is silent"
        );
    }

    #[test]
    fn reports_a_quiet_file_with_the_gain_to_raise_it() {
        assert_eq!(
            texts(&report(-166, -32)),
            [
                "Integrated loudness   -16.6 LUFS  FAIL  target -14.0 ±1.0 LUFS, raise by +2.6 dB",
                "True peak              -3.2 dBTP  PASS  max -1.0 dBTP",
                "Loudness range          6.1 LU",
            ]
        );
    }

    #[test]
    fn reports_a_loud_file_with_the_gain_to_lower_it() {
        assert_eq!(
            texts(&report(-93, 4)),
            [
                "Integrated loudness    -9.3 LUFS  FAIL  target -14.0 ±1.0 LUFS, lower by -4.7 dB",
                "True peak               0.4 dBTP  FAIL  max -1.0 dBTP",
                "Loudness range          6.1 LU",
            ]
        );
    }

    #[test]
    fn passes_integrated_loudness_exactly_one_lu_from_the_target() {
        assert_eq!(report(-150, -20).integrated_verdict(), Verdict::Pass);
        assert_eq!(report(-130, -20).integrated_verdict(), Verdict::Pass);
    }

    #[test]
    fn fails_integrated_loudness_just_over_one_lu_from_the_target() {
        assert_eq!(report(-151, -20).integrated_verdict(), Verdict::Fail);
        assert_eq!(report(-129, -20).integrated_verdict(), Verdict::Fail);
    }

    #[test]
    fn passes_a_true_peak_of_exactly_minus_one() {
        assert_eq!(report(-140, -10).true_peak_verdict(), Verdict::Pass);
    }

    #[test]
    fn fails_a_true_peak_just_over_minus_one() {
        assert_eq!(report(-140, -9).true_peak_verdict(), Verdict::Fail);
    }

    #[test]
    fn flags_a_report_when_either_check_fails() {
        assert!(!report(-140, -10).flagged());
        assert!(report(-160, -10).flagged());
        assert!(report(-140, -9).flagged());
    }

    #[test]
    fn rounds_to_the_nearest_tenth_before_checking() {
        assert_eq!(Tenths::round(-15.04), Some(Tenths(-150)));
        assert_eq!(Tenths::round(f64::NEG_INFINITY), None);
    }

    #[test]
    fn formats_tenths() {
        assert_eq!(Tenths(-5).to_string(), "-0.5");
        assert_eq!(format!("{:+}", Tenths(26)), "+2.6");
        assert_eq!(format!("{:+}", Tenths(-47)), "-4.7");
    }

    #[test]
    fn describes_progress() {
        let progress = |total| Progress {
            done: Duration::from_secs(3 * 3600),
            total,
        };

        assert_eq!(
            progress(Some(Duration::from_secs(8 * 3600))).to_string(),
            "Measuring… 38% · 3:00:00 of 8:00:00"
        );
        assert_eq!(progress(None).to_string(), "Measuring… 3:00:00");
    }
}
