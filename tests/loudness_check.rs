//! Runs `vt loudness-check` on sine waves generated at known levels.

use std::{path::Path, process::Command};

use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::TempDir;

/// ffmpeg's `sine` source peaks at 1/8 of full scale.
const SINE_PEAK_DBFS: f64 = -18.06;

/// A mono sine reads 3.01 dB below its peak, which is ITU-R BS.1770's calibration.
const SINE_LUFS_BELOW_PEAK: f64 = 3.01;

/// Writes 20 seconds of a mono 997 Hz sine at `lufs` integrated loudness to `path`.
fn sine(path: &Path, lufs: f64) {
    let gain = lufs + SINE_LUFS_BELOW_PEAK - SINE_PEAK_DBFS;
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi"])
        .args(["-i", "sine=frequency=997:sample_rate=48000:duration=20"])
        .args(["-af", &format!("volume={gain:.2}dB")])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// Measures a sine generated at `lufs`, and returns the exit code and the integrated loudness
/// `vt loudness-check` printed.
fn measure(lufs: f64) -> (i32, f64) {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("sine.wav");
    sine(&file, lufs);

    let output = cargo_bin_cmd!("vt")
        .arg("loudness-check")
        .arg(&file)
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let integrated = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Integrated loudness"))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no integrated loudness in {stdout:?}"));
    (output.status.code().unwrap(), integrated.parse().unwrap())
}

#[test]
fn passes_a_file_at_the_target() {
    let (exit, integrated) = measure(-14.0);

    assert_eq!(exit, 0);
    assert!((integrated - -14.0).abs() <= 0.2, "measured {integrated}");
}

#[test]
fn flags_a_quiet_file() {
    let (exit, integrated) = measure(-24.0);

    assert_eq!(exit, 1);
    assert!((integrated - -24.0).abs() <= 0.2, "measured {integrated}");
}
