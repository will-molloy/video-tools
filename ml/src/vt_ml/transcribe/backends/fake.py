"""A deterministic stand-in for Whisper, for tests: every loud stretch is a segment of speech."""

import os
import time
from collections.abc import Callable

import numpy as np

from vt_ml.errors import UsageError
from vt_ml.transcribe.backends import SAMPLE_RATE
from vt_ml.transcribe.segment import Segment

# Seconds to sleep per progress step, so tests can cancel mid-run.
DELAY_ENV = "VT_ML_FAKE_DELAY_S"

TEXT = "speech"

# 10 ms frames whose RMS is above the threshold are loud.
FRAME = SAMPLE_RATE // 100
THRESHOLD = 0.05

# Progress is reported once per second of audio.
STEP = SAMPLE_RATE


class FakeBackend:
    def __init__(self, device: str):
        if device != "cpu":
            raise UsageError(f"the fake backend runs on cpu, not {device}")
        self.device = "cpu"
        self._delay = float(os.environ.get(DELAY_ENV, "0"))

    def transcribe(
        self, audio: np.ndarray, progress: Callable[[int, int], None]
    ) -> list[Segment]:
        total_ms = len(audio) * 1000 // SAMPLE_RATE
        for start in range(0, len(audio), STEP):
            time.sleep(self._delay)
            progress(min(start + STEP, len(audio)) * 1000 // SAMPLE_RATE, total_ms)

        frames = len(audio) // FRAME
        rms = np.sqrt(np.mean(audio[: frames * FRAME].reshape(frames, FRAME) ** 2, axis=1))
        loud = np.concatenate([[False], rms > THRESHOLD, [False]])
        edges = np.flatnonzero(np.diff(loud.astype(np.int8)))
        return [
            Segment(start * FRAME / SAMPLE_RATE, end * FRAME / SAMPLE_RATE, TEXT)
            for start, end in zip(edges[::2], edges[1::2])
        ]

    def peak_vram_mb(self) -> float | None:
        return None
