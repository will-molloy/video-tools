"""Streams a WAV through a backend chunk by chunk, writing both stems as it goes.

Only the speech spans go to the backend. Outside them voice is silence and ambience is the input.
Inside a span the stems are the backend's, faded in and out over the span's first and last
`FADE_S`, so the two regions meet without a click. A span that touches the start or end of the
file doesn't fade there, since nothing lies outside it.
"""

from collections.abc import Callable, Sequence
from pathlib import Path

import numpy as np
import soundfile as sf

from vt_ml.errors import UsageError
from vt_ml.separate import chunking
from vt_ml.separate.backends import Backend

VOICE = "voice.wav"
AMBIENCE = "ambience.wav"

# The padding `vt` adds around each line of speech, which the fade stays within.
FADE_S = 0.25

# Frames copied at a time outside the spans, in seconds of audio.
COPY_BLOCK_S = 10


def separate_file(
    input_path: Path,
    output_dir: Path,
    backend: Backend,
    spans: Sequence[tuple[float, float]],
    on_progress: Callable[[int, int], None],
) -> tuple[Path, Path]:
    """Writes `voice.wav` and `ambience.wav` to `output_dir` and returns their paths.

    `spans` are the (start, end) seconds of speech. Removes both stems if anything goes wrong,
    including SIGTERM, so a failed run leaves nothing behind. Progress counts one unit per channel
    per chunk, over the chunks inside the spans.
    """
    voice_path = output_dir / VOICE
    ambience_path = output_dir / AMBIENCE
    try:
        with sf.SoundFile(input_path) as source:
            _separate(source, voice_path, ambience_path, backend, spans, on_progress)
    except BaseException:
        voice_path.unlink(missing_ok=True)
        ambience_path.unlink(missing_ok=True)
        raise
    return voice_path, ambience_path


def regions(spans: Sequence[tuple[float, float]], rate: int, frames: int) -> list[tuple[int, int]]:
    """Sample ranges of the spans, clamped to the file, sorted, and merged where they touch."""
    ranges = sorted(
        (max(0, round(start * rate)), min(frames, round(end * rate))) for start, end in spans
    )
    merged: list[list[int]] = []
    for start, end in ranges:
        if end <= start:
            continue
        if merged and start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    return [(start, end) for start, end in merged]


def _separate(
    source: sf.SoundFile,
    voice_path: Path,
    ambience_path: Path,
    backend: Backend,
    spans: Sequence[tuple[float, float]],
    on_progress: Callable[[int, int], None],
) -> None:
    if source.samplerate != backend.sample_rate:
        raise UsageError(
            f"input is {source.samplerate} Hz, but this backend needs {backend.sample_rate} Hz"
        )
    frames, channels = source.frames, source.channels
    speech = regions(spans, source.samplerate, frames)
    plans = [chunking.plan(end - start, backend.chunk_length, backend.overlap) for start, end in speech]
    total = sum(len(plan) for plan in plans) * channels

    # RF64 lifts WAV's 4 GB limit, which float32 stereo passes after about 3 hours at 48 kHz.
    # Float samples keep stems that exceed full scale from clipping.
    def open_stem(path: Path) -> sf.SoundFile:
        return sf.SoundFile(
            path, "w", samplerate=source.samplerate, channels=channels, format="RF64", subtype="FLOAT"
        )

    with open_stem(voice_path) as voice_out, open_stem(ambience_path) as ambience_out:
        position = 0
        done = 0
        for (start, end), plan in zip(speech, plans):
            _copy_outside(source, position, start, voice_out, ambience_out)
            done = _separate_span(
                source, start, end, plan, backend, voice_out, ambience_out, done, total, on_progress
            )
            position = end
        _copy_outside(source, position, frames, voice_out, ambience_out)


def _copy_outside(
    source: sf.SoundFile, start: int, end: int, voice_out: sf.SoundFile, ambience_out: sf.SoundFile
) -> None:
    """Writes silence as voice and the input as ambience for the samples in [start, end)."""
    block_frames = COPY_BLOCK_S * source.samplerate
    source.seek(start)
    for at in range(start, end, block_frames):
        block = source.read(min(block_frames, end - at), dtype="float32", always_2d=True)
        voice_out.write(np.zeros_like(block))
        ambience_out.write(block)


def _separate_span(
    source: sf.SoundFile,
    start: int,
    end: int,
    chunks: list[chunking.Chunk],
    backend: Backend,
    voice_out: sf.SoundFile,
    ambience_out: sf.SoundFile,
    done: int,
    total: int,
    on_progress: Callable[[int, int], None],
) -> int:
    """Separates [start, end) and returns the progress count afterwards."""
    length, channels = end - start, source.channels
    fade = round(FADE_S * source.samplerate)
    fade = min(fade, length // 2)
    fade_in = fade if start > 0 else 0
    fade_out = fade if end < source.frames else 0
    up = chunking.ramp(fade) if fade else np.zeros(0)

    voice_blend = chunking.OverlapAdd(chunks, length, backend.overlap, channels)
    ambience_blend = chunking.OverlapAdd(chunks, length, backend.overlap, channels)
    written = 0
    for i, chunk in enumerate(chunks):
        source.seek(start + chunk.start)
        block = source.read(chunk.length, dtype="float32", always_2d=True)
        voice = np.empty_like(block)
        ambience = np.empty_like(block)

        # Mono models get one channel at a time, so stereo keeps its image.
        for c in range(channels):
            voice[:, c], ambience[:, c] = backend.separate(block[:, c], i)
            done += 1
            on_progress(done, total)

        voice = voice_blend.add(voice)
        ambience = ambience_blend.add(ambience)
        gain = _gain(written, len(voice), length, up, fade_in, fade_out)[:, None]
        source.seek(start + written)
        original = source.read(len(voice), dtype="float32", always_2d=True)
        voice_out.write((voice * gain).astype(np.float32))
        ambience_out.write((ambience * gain + original * (1 - gain)).astype(np.float32))
        written += len(voice)
    return done


def _gain(
    at: int, count: int, length: int, up: np.ndarray, fade_in: int, fade_out: int
) -> np.ndarray:
    """How much of the separated audio to use for span samples [at, at + count)."""
    index = np.arange(at, at + count)
    gain = np.ones(count, dtype=np.float64)
    rising = index < fade_in
    gain[rising] = up[index[rising]]
    falling = index >= length - fade_out
    gain[falling] *= up[::-1][index[falling] - (length - fade_out)]
    return gain
