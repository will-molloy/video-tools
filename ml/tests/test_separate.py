"""Runs `vt-ml separate` as `vt` does, in a subprocess, with the fake backend."""

import json
import os
import signal
import subprocess
import sys
from pathlib import Path

import numpy as np
import soundfile as sf

from vt_ml.separate import chunking
from vt_ml.separate.backends.fake import AMBIENCE_GAIN, DELAY_ENV
from vt_ml.separate.pipeline import FADE_S

RATE = 44_100

WHOLE_FILE = ((0.0, 1000.0),)


def write_wav(path: Path, samples: np.ndarray, rate: int = RATE) -> Path:
    sf.write(path, samples, rate, subtype="FLOAT")
    return path


def stereo(seconds: float) -> np.ndarray:
    """A different tone in each channel, so swapped or mixed channels show."""
    t = np.arange(int(seconds * RATE)) / RATE
    return np.stack([np.sin(2 * np.pi * 440 * t), 0.5 * np.sin(2 * np.pi * 660 * t)], 1).astype(
        np.float32
    )


def constant(seconds: float, level: float = 0.5) -> np.ndarray:
    return np.full((int(seconds * RATE), 2), level, dtype=np.float32)


def command(
    input_path: Path,
    output_dir: Path,
    spans=WHOLE_FILE,
    *extra: str,
    backend: str = "fake",
    device: str = "cpu",
) -> list[str]:
    speech = output_dir.parent / f"{output_dir.name}-speech.json"
    speech.write_text(json.dumps(spans))
    return [
        sys.executable,
        "-m",
        "vt_ml",
        "separate",
        "--input",
        str(input_path),
        "--output-dir",
        str(output_dir),
        "--speech",
        str(speech),
        "--backend",
        backend,
        "--device",
        device,
        *extra,
    ]


def run(args: list[str]):
    result = subprocess.run(args, capture_output=True, text=True, timeout=60)
    return result.returncode, [json.loads(line) for line in result.stdout.splitlines()]


def stems(tmp_path: Path) -> tuple[np.ndarray, np.ndarray]:
    voice, _ = sf.read(tmp_path / "out" / "voice.wav", dtype="float32")
    ambience, _ = sf.read(tmp_path / "out" / "ambience.wav", dtype="float32")
    return voice, ambience


def separate(tmp_path: Path, audio: np.ndarray, spans) -> tuple[np.ndarray, np.ndarray]:
    out = tmp_path / "out"
    out.mkdir()
    code, messages = run(command(write_wav(tmp_path / "in.wav", audio), out, spans))
    assert code == 0, messages
    return stems(tmp_path)


def test_separates_stereo_with_the_fake_backend(tmp_path):
    signal_in = stereo(3)
    source = write_wav(tmp_path / "in.wav", signal_in)
    out = tmp_path / "out"
    out.mkdir()

    code, messages = run(command(source, out))

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
        "voice": str(out / "voice.wav"),
        "ambience": str(out / "ambience.wav"),
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
    out = tmp_path / "out"
    out.mkdir()

    run(command(source, out))

    info = sf.info(out / "voice.wav")
    assert (info.format, info.subtype, info.channels) == ("RF64", "FLOAT", 2)


def test_leaves_the_audio_outside_the_speech_untouched(tmp_path):
    signal_in = stereo(6)

    voice, ambience = separate(tmp_path, signal_in, [(2.0, 4.0)])

    outside = np.r_[0 : int((2.0 - FADE_S) * RATE), int((4.0 + FADE_S) * RATE) : len(signal_in)]
    assert voice.shape == signal_in.shape
    np.testing.assert_array_equal(voice[outside], np.zeros_like(voice[outside]))
    np.testing.assert_array_equal(ambience[outside], signal_in[outside])


def test_uses_the_models_stems_inside_the_speech(tmp_path):
    signal_in = stereo(6)

    voice, ambience = separate(tmp_path, signal_in, [(2.0, 4.0)])

    inside = slice(int((2.0 + FADE_S) * RATE), int((4.0 - FADE_S) * RATE))
    np.testing.assert_allclose(voice[inside], signal_in[inside], rtol=0, atol=1e-6)
    np.testing.assert_allclose(
        ambience[inside], signal_in[inside] * AMBIENCE_GAIN, rtol=0, atol=1e-6
    )


def test_fades_over_a_span_edge_with_the_chunking_ramp(tmp_path):
    level = 0.5
    start, end = 2.0, 4.0

    voice, ambience = separate(tmp_path, constant(6, level), [(start, end)])

    fade = round(FADE_S * RATE)
    up = chunking.ramp(fade)
    head = slice(round(start * RATE), round(start * RATE) + fade)
    tail = slice(round(end * RATE) - fade, round(end * RATE))
    np.testing.assert_allclose(voice[head, 0], level * up, rtol=0, atol=1e-6)
    np.testing.assert_allclose(voice[tail, 0], level * up[::-1], rtol=0, atol=1e-6)
    np.testing.assert_allclose(
        ambience[head, 0], level * (AMBIENCE_GAIN * up + (1 - up)), rtol=0, atol=1e-6
    )


def test_does_not_fade_where_a_span_touches_the_file_edge(tmp_path):
    signal_in = stereo(4)

    voice, _ = separate(tmp_path, signal_in, [(0.0, 1000.0)])

    np.testing.assert_allclose(voice, signal_in, rtol=0, atol=1e-6)


def test_merges_overlapping_spans(tmp_path):
    signal_in = stereo(6)

    voice, _ = separate(tmp_path, signal_in, [(1.0, 3.0), (2.0, 4.0)])

    inside = slice(int((1.0 + FADE_S) * RATE), int((4.0 - FADE_S) * RATE))
    np.testing.assert_allclose(voice[inside], signal_in[inside], rtol=0, atol=1e-6)


def test_sends_only_the_speech_to_the_model(tmp_path):
    source = write_wav(tmp_path / "in.wav", stereo(20))
    out = tmp_path / "out"
    out.mkdir()

    _, messages = run(command(source, out, [(2.0, 5.0), (12.0, 13.0)]))

    # The fake model takes 2 s chunks that overlap by 0.5 s. A 3 s span needs two and a 1 s span
    # one, each for two channels.
    progress = [m for m in messages if m["type"] == "progress"]
    assert progress[-1] == {"type": "progress", "done": 6, "total": 6}


def test_separates_nothing_without_speech(tmp_path):
    signal_in = stereo(3)
    out = tmp_path / "out"
    out.mkdir()

    code, messages = run(command(write_wav(tmp_path / "in.wav", signal_in), out, []))

    assert code == 0
    assert [m for m in messages if m["type"] == "progress"] == []
    voice, ambience = stems(tmp_path)
    np.testing.assert_array_equal(voice, np.zeros_like(signal_in))
    np.testing.assert_array_equal(ambience, signal_in)


def test_reports_a_missing_input(tmp_path):
    out = tmp_path / "out"
    out.mkdir()

    code, messages = run(command(tmp_path / "missing.wav", out))

    assert code == 2
    assert messages == [
        {"type": "error", "message": f"input {tmp_path / 'missing.wav'} does not exist"}
    ]


def test_reports_a_missing_output_directory(tmp_path):
    source = write_wav(tmp_path / "in.wav", stereo(1))
    missing = tmp_path / "missing"
    args = command(source, missing)

    code, messages = run(args)

    assert code == 2
    assert messages == [
        {"type": "error", "message": f"output directory {missing} does not exist"}
    ]


def test_reports_a_bad_speech_span(tmp_path):
    source = write_wav(tmp_path / "in.wav", stereo(1))
    out = tmp_path / "out"
    out.mkdir()

    code, messages = run(command(source, out, [(3.0, 2.0)]))

    assert code == 2
    assert messages == [
        {"type": "error", "message": "speech span [3.0, 2.0] must satisfy 0 <= start < end"}
    ]


def test_refuses_to_move_the_fake_backend_off_the_cpu(tmp_path):
    source = write_wav(tmp_path / "in.wav", stereo(1))
    out = tmp_path / "out"
    out.mkdir()

    code, messages = run(command(source, out, device="cuda"))

    assert code == 2
    assert messages == [{"type": "error", "message": "the fake backend runs on cpu, not cuda"}]


def test_rejects_the_wrong_sample_rate_and_leaves_no_outputs(tmp_path):
    source = write_wav(tmp_path / "in.wav", stereo(1), rate=48_000)
    out = tmp_path / "out"
    out.mkdir()

    code, messages = run(command(source, out))

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
        command(source, out),
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
