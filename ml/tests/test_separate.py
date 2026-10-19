"""Runs `vt-ml separate` as `vt` does, in a subprocess, with the fake backend."""

import json
import os
import signal
import subprocess
import sys
from pathlib import Path

import numpy as np
import soundfile as sf

from vt_ml.separate.backends.fake import AMBIENCE_GAIN, DELAY_ENV

RATE = 44_100


def write_wav(path: Path, samples: np.ndarray, rate: int = RATE) -> Path:
    sf.write(path, samples, rate, subtype="FLOAT")
    return path


def stereo(seconds: float) -> np.ndarray:
    """A different tone in each channel, so swapped or mixed channels show."""
    t = np.arange(int(seconds * RATE)) / RATE
    return np.stack([np.sin(2 * np.pi * 440 * t), 0.5 * np.sin(2 * np.pi * 660 * t)], 1).astype(
        np.float32
    )


def command(input_path: Path, output_dir: Path, *extra: str) -> list[str]:
    return [
        sys.executable,
        "-m",
        "vt_ml",
        "separate",
        "--input",
        str(input_path),
        "--output-dir",
        str(output_dir),
        *extra,
    ]


def run(args: list[str]):
    result = subprocess.run(args, capture_output=True, text=True, timeout=60)
    return result.returncode, [json.loads(line) for line in result.stdout.splitlines()]


def test_separates_stereo_with_the_fake_backend(tmp_path):
    signal_in = stereo(3)
    source = write_wav(tmp_path / "in.wav", signal_in)

    code, messages = run(command(source, tmp_path, "--backend", "fake", "--device", "cpu"))

    assert code == 0
    done = messages[-1]
    assert messages[:-1] == [
        {"type": "log", "message": "Separating on cpu"},
        # Two chunks of two channels.
        {"type": "progress", "done": 1, "total": 4},
        {"type": "progress", "done": 2, "total": 4},
        {"type": "progress", "done": 3, "total": 4},
        {"type": "progress", "done": 4, "total": 4},
    ]
    assert done == {
        "type": "done",
        "voice": str(tmp_path / "voice.wav"),
        "ambience": str(tmp_path / "ambience.wav"),
        "device": "cpu",
        "elapsed_s": done["elapsed_s"],
        "peak_vram_mb": None,
    }
    voice, voice_rate = sf.read(done["voice"], dtype="float32")
    ambience, _ = sf.read(done["ambience"], dtype="float32")
    assert voice_rate == RATE
    np.testing.assert_allclose(voice, signal_in, rtol=0, atol=1e-6)
    np.testing.assert_allclose(ambience, signal_in * AMBIENCE_GAIN, rtol=0, atol=1e-6)


def test_writes_rf64_float_stems(tmp_path):
    source = write_wav(tmp_path / "in.wav", stereo(1))

    run(command(source, tmp_path, "--backend", "fake", "--device", "cpu"))

    info = sf.info(tmp_path / "voice.wav")
    assert (info.format, info.subtype, info.channels) == ("RF64", "FLOAT", 2)


def test_reports_a_missing_input(tmp_path):
    code, messages = run(
        command(tmp_path / "missing.wav", tmp_path, "--backend", "fake", "--device", "cpu")
    )

    assert code == 2
    assert messages == [
        {"type": "error", "message": f"input {tmp_path / 'missing.wav'} does not exist"}
    ]


def test_reports_a_missing_output_directory(tmp_path):
    source = write_wav(tmp_path / "in.wav", stereo(1))

    code, messages = run(
        command(source, tmp_path / "missing", "--backend", "fake", "--device", "cpu")
    )

    assert code == 2
    assert messages == [
        {
            "type": "error",
            "message": f"output directory {tmp_path / 'missing'} does not exist",
        }
    ]


def test_refuses_to_move_the_fake_backend_off_the_cpu(tmp_path):
    source = write_wav(tmp_path / "in.wav", stereo(1))

    code, messages = run(command(source, tmp_path, "--backend", "fake", "--device", "cuda"))

    assert code == 2
    assert messages == [{"type": "error", "message": "the fake backend runs on cpu, not cuda"}]


def test_rejects_the_wrong_sample_rate_and_leaves_no_outputs(tmp_path):
    source = write_wav(tmp_path / "in.wav", stereo(1), rate=48_000)
    out = tmp_path / "out"
    out.mkdir()

    code, messages = run(command(source, out, "--backend", "fake", "--device", "cpu"))

    assert code == 2
    assert messages[-1] == {
        "type": "error",
        "message": "input is 48000 Hz, but this backend needs 44100 Hz",
    }
    assert list(out.iterdir()) == []


def test_stops_on_sigterm_and_removes_partial_outputs(tmp_path):
    source = write_wav(tmp_path / "in.wav", stereo(30))
    out = tmp_path / "out"
    out.mkdir()
    child = subprocess.Popen(
        command(source, out, "--backend", "fake", "--device", "cpu"),
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env={**os.environ, DELAY_ENV: "0.2"},
    )

    # Wait until it's mid-run, with both stems open for writing.
    for line in child.stdout:
        if json.loads(line)["type"] == "progress":
            break
    assert sorted(p.name for p in out.iterdir()) == ["ambience.wav", "voice.wav"]
    child.send_signal(signal.SIGTERM)
    rest = [json.loads(line) for line in child.stdout]

    assert child.wait(timeout=10) == 143
    assert all(m["type"] == "progress" for m in rest)
    assert list(out.iterdir()) == []
