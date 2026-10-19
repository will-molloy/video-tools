"""The JSON-lines protocol `vt` reads from stdout. `ml/README.md` documents it."""

import json
import os
import sys
from collections.abc import Mapping
from dataclasses import dataclass
from typing import IO

EXIT_OK = 0
EXIT_ERROR = 2
EXIT_TERMINATED = 143


@dataclass(frozen=True)
class Stats:
    """How a stage ran, reported alongside its result."""

    device: str
    elapsed_s: float

    # None on CPU.
    peak_vram_mb: float | None


class Protocol:
    """Writes one JSON object per line and flushes each, so `vt` sees progress as it happens."""

    def __init__(self, out: IO[str]):
        self._out = out

    def progress(self, done: int, total: int) -> None:
        self._emit({"type": "progress", "done": done, "total": total})

    def log(self, message: str) -> None:
        self._emit({"type": "log", "message": message})

    def done(self, result: Mapping[str, object], stats: Stats) -> None:
        """Reports success. `result` holds the stage's own fields."""
        self._emit(
            {
                "type": "done",
                **result,
                "device": stats.device,
                "elapsed_s": round(stats.elapsed_s, 3),
                "peak_vram_mb": None
                if stats.peak_vram_mb is None
                else round(stats.peak_vram_mb, 1),
            }
        )

    def error(self, message: str) -> None:
        self._emit({"type": "error", "message": message})

    def _emit(self, message: Mapping[str, object]) -> None:
        self._out.write(json.dumps(message) + "\n")
        self._out.flush()


def claim_stdout() -> Protocol:
    """Reserves the process's stdout for the protocol and points fd 1 at stderr.

    Libraries print to stdout freely, from Python and from C, and one stray line would corrupt
    the stream. Duplicating the original stdout first keeps a private handle to it.
    """
    sys.stdout.flush()
    out = os.fdopen(os.dup(sys.stdout.fileno()), "w", encoding="utf-8")
    os.dup2(sys.stderr.fileno(), sys.stdout.fileno())
    # Python's stdout buffers separately, so writes through it would land out of order.
    sys.stdout = sys.stderr
    return Protocol(out)
