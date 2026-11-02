from collections.abc import Callable
from enum import StrEnum
from typing import Protocol

import numpy as np

from vt_ml.transcribe.segment import Segment

# Whisper's rate. The fake backend uses it too, so both read the same input.
SAMPLE_RATE = 16_000


class Name(StrEnum):
    WHISPER = "whisper"
    FAKE = "fake"


class Backend(Protocol):
    device: str

    def transcribe(
        self, audio: np.ndarray, progress: Callable[[int, int], None]
    ) -> list[Segment]:
        """Finds the speech in mono `audio`, reporting milliseconds read out of the total."""

    def peak_vram_mb(self) -> float | None: ...


def load(name: Name, device: str, log: Callable[[str], None]) -> Backend:
    # Imported here, so the fake backend never imports torch.
    match name:
        case Name.WHISPER:
            from vt_ml.transcribe.backends.whisper import WhisperBackend

            return WhisperBackend(device, log)
        case Name.FAKE:
            from vt_ml.transcribe.backends.fake import FakeBackend

            return FakeBackend(device)
