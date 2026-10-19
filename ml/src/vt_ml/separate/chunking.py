"""Splits long audio into overlapping chunks and blends the separated chunks back together.

Each chunk is weighted by a window that ramps up over its first `overlap` samples and down over
its last. Neighbouring ramps are complementary (sin² and cos² of the same angle), so wherever two
chunks overlap their weights sum to 1 and a crossfade replaces what would otherwise be a hard cut.
Output samples are divided by the summed weights, which makes the blend exact even where the
weights don't sum to 1: at the file's edges, and where the last chunk overlaps its neighbour by
more than `overlap`.

The blend is streamed. A sample is final once no later chunk covers it, which is everything
before the next chunk's start, so memory stays at about one chunk however long the file is.
"""

from dataclasses import dataclass

import numpy as np


@dataclass(frozen=True)
class Chunk:
    start: int
    length: int

    @property
    def end(self) -> int:
        return self.start + self.length


def plan(frames: int, length: int, overlap: int) -> list[Chunk]:
    """Covers `frames` samples with chunks of `length` that overlap by at least `overlap`.

    Every chunk is full length, so the model always sees full context. The last one is aligned
    to the end of the file, so it can overlap its neighbour by more than `overlap`.
    """
    if not 0 <= overlap < length:
        raise ValueError(f"overlap {overlap} must be in [0, {length})")
    if frames <= length:
        return [Chunk(0, frames)] if frames > 0 else []
    hop = length - overlap
    starts = list(range(0, frames - length, hop)) + [frames - length]
    return [Chunk(start, length) for start in starts]


def ramp(overlap: int) -> np.ndarray:
    """Rising weights over `overlap` samples. `ramp(n)[::-1]` is `1 - ramp(n)`, so they crossfade."""
    n = np.arange(overlap, dtype=np.float64)
    return np.sin(np.pi / 2 * (n + 0.5) / overlap) ** 2


def window(chunk: Chunk, frames: int, overlap: int) -> np.ndarray:
    """A chunk's blend weights. Edges at the file's start or end don't ramp."""
    weights = np.ones(chunk.length, dtype=np.float64)
    n = min(overlap, chunk.length)
    if n == 0:
        return weights
    up = ramp(n)
    if chunk.start > 0:
        weights[:n] *= up
    if chunk.end < frames:
        weights[-n:] *= up[::-1]
    return weights


class OverlapAdd:
    """Blends separated chunks, given in `plan` order, into a stream of final samples."""

    def __init__(self, chunks: list[Chunk], frames: int, overlap: int, channels: int):
        self._chunks = chunks
        self._frames = frames
        self._overlap = overlap
        self._next = 0

        # Pending samples start at `_flushed`.
        self._flushed = 0
        self._sum = np.zeros((0, channels), dtype=np.float64)
        self._weight = np.zeros(0, dtype=np.float64)

    def add(self, block: np.ndarray) -> np.ndarray:
        """Takes the next chunk's output, shaped (length, channels), and returns the samples that
        no later chunk overlaps."""
        chunk = self._chunks[self._next]
        if block.shape[0] != chunk.length:
            raise ValueError(f"chunk {self._next} has {block.shape[0]} samples, expected {chunk.length}")
        self._next += 1

        pending = chunk.end - self._flushed
        if pending > len(self._weight):
            grow = pending - len(self._weight)
            self._sum = np.concatenate([self._sum, np.zeros((grow, self._sum.shape[1]))])
            self._weight = np.concatenate([self._weight, np.zeros(grow)])

        weights = window(chunk, self._frames, self._overlap)
        at = chunk.start - self._flushed
        self._sum[at : at + chunk.length] += block * weights[:, None]
        self._weight[at : at + chunk.length] += weights

        final_until = (
            self._chunks[self._next].start if self._next < len(self._chunks) else self._frames
        )
        n = final_until - self._flushed
        out = self._sum[:n] / self._weight[:n, None]
        self._sum = self._sum[n:]
        self._weight = self._weight[n:]
        self._flushed = final_until
        return out.astype(np.float32)
