"""Runs `vt-ml transcribe` as `vt` does, in a subprocess, with the fake backend."""

import json
import os
import signal
import subprocess
import sys
from pathlib import Path

import numpy as np
import soundfile as sf

from vt_ml.transcribe.backends.fake import DELAY_ENV

RATE = 16_000


def bursts(seconds: float, *spans: tuple[float, float]) -> np.ndarray:
    """Silence with a loud tone over each (start, end) span, in seconds."""
    gate = np.zeros(int(seconds * RATE), dtype=np.float32)
    for start, end in spans:
        gate[int(start * RATE) : int(end * RATE)] = 1
    t = np.arange(len(gate)) / RATE
    return gate * 0.5 * np.sin(2 * np.pi * 440 * t).astype(np.float32)


def write_wav(path: Path, audio: np.ndarray, rate: int = RATE) -> Path:
    sf.write(path, audio, rate, subtype="FLOAT")
    return path


def command(input_path: Path, *extra: str) -> list[str]:
    return [
        sys.executable,
        "-m",
        "vt_ml",
        "transcribe",
        "--input",
        str(input_path),
        "--backend",
        "fake",
        "--device",
        "cpu",
        *extra,
    ]


def run(args: list[str]):
    result = subprocess.run(args, capture_output=True, text=True, timeout=60)
    return result.returncode, [json.loads(line) for line in result.stdout.splitlines()]


def test_reports_each_loud_stretch_as_a_segment(tmp_path):
    source = write_wav(tmp_path / "in.wav", bursts(2.5, (0.2, 0.7), (1.5, 2.0)))

    code, messages = run(command(source))

    assert code == 0
    done = messages[-1]
    assert messages[:-1] == [
        {"type": "log", "message": "Transcribing on cpu"},
        {"type": "progress", "done": 1000, "total": 2500},
        {"type": "progress", "done": 2000, "total": 2500},
        {"type": "progress", "done": 2500, "total": 2500},
    ]
    assert done == {
        "type": "done",
        "segments": [
            {"start": 0.2, "end": 0.7, "text": "speech"},
            {"start": 1.5, "end": 2.0, "text": "speech"},
        ],
        "device": "cpu",
        "elapsed_s": done["elapsed_s"],
        "peak_vram_mb": None,
    }


def test_reports_silence_as_no_segments(tmp_path):
    source = write_wav(tmp_path / "in.wav", bursts(1.0))

    _, messages = run(command(source))

    assert messages[-1]["segments"] == []


def test_rejects_stereo(tmp_path):
    source = write_wav(tmp_path / "in.wav", np.zeros((RATE, 2), dtype=np.float32))

    code, messages = run(command(source))

    assert code == 2
    assert messages == [
        {"type": "error", "message": "input has 2 channels, but transcription needs mono"}
    ]


def test_rejects_the_wrong_sample_rate(tmp_path):
    source = write_wav(tmp_path / "in.wav", bursts(1.0), rate=44_100)

    code, messages = run(command(source))

    assert code == 2
    assert messages == [
        {"type": "error", "message": "input is 44100 Hz, but transcription needs 16000 Hz"}
    ]


def test_reports_a_missing_input(tmp_path):
    code, messages = run(command(tmp_path / "missing.wav"))

    assert code == 2
    assert messages == [
        {"type": "error", "message": f"input {tmp_path / 'missing.wav'} does not exist"}
    ]


def test_refuses_to_move_the_fake_backend_off_the_cpu(tmp_path):
    source = write_wav(tmp_path / "in.wav", bursts(1.0))
    args = command(source)
    args[args.index("cpu")] = "cuda"

    code, messages = run(args)

    assert code == 2
    assert messages[-1] == {"type": "error", "message": "the fake backend runs on cpu, not cuda"}


def test_stops_on_sigterm(tmp_path):
    source = write_wav(tmp_path / "in.wav", bursts(30.0))
    child = subprocess.Popen(
        command(source),
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env={**os.environ, DELAY_ENV: "0.2"},
    )

    for line in child.stdout:
        if json.loads(line)["type"] == "progress":
            break
    child.send_signal(signal.SIGTERM)
    rest = [json.loads(line) for line in child.stdout]

    assert child.wait(timeout=10) == 143
    assert all(m["type"] == "progress" for m in rest)
