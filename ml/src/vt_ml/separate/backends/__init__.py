"""Separation models behind one interface. Each separates a mono chunk at its native sample rate."""

from enum import StrEnum
from typing import Protocol

import numpy as np


class Name(StrEnum):
    SAM = "sam"
    TIGER = "tiger"
    FAKE = "fake"


class Backend(Protocol):
    sample_rate: int

    # Chunk length and minimum overlap, in samples.
    chunk_length: int
    overlap: int

    # Describes the device for the `done` message, e.g. "cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2)".
    device: str

    def separate(self, mono: np.ndarray, chunk_index: int) -> tuple[np.ndarray, np.ndarray]:
        """Returns (voice, ambience) for a float32 chunk, each the same length as `mono`."""
        ...

    def peak_vram_mb(self) -> float | None: ...


def load(name: Name, device: str, log) -> Backend:
    """Imports lazily, so a backend's dependencies are only needed when it runs."""
    match name:
        case Name.SAM:
            from vt_ml.separate.backends.sam import SamBackend

            return SamBackend(device, log)
        case Name.TIGER:
            from vt_ml.separate.backends.tiger import TigerBackend

            return TigerBackend(device, log)
        case Name.FAKE:
            from vt_ml.separate.backends.fake import FakeBackend

            return FakeBackend(device)
