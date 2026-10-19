"""A deterministic stand-in with no ML dependencies, for CI and integration tests."""

import os
import time

import numpy as np

from vt_ml.errors import UsageError

# Seconds to sleep per chunk, so tests can cancel mid-run.
DELAY_ENV = "VT_ML_FAKE_DELAY_S"

AMBIENCE_GAIN = 0.25


class FakeBackend:
    # Differs from the fixtures' 48 kHz, so tests exercise resampling both ways.
    sample_rate = 44_100
    chunk_length = 2 * 44_100
    overlap = 44_100 // 2

    def __init__(self, device: str):
        if device != "cpu":
            raise UsageError(f"the fake backend runs on cpu, not {device}")
        self.device = "cpu"
        self._delay = float(os.environ.get(DELAY_ENV, "0"))

    def separate(self, mono: np.ndarray, chunk_index: int) -> tuple[np.ndarray, np.ndarray]:
        if self._delay:
            time.sleep(self._delay)
        return mono.copy(), mono * AMBIENCE_GAIN

    def peak_vram_mb(self) -> float | None:
        return None
