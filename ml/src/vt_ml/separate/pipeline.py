"""Streams a WAV through a backend chunk by chunk, writing both stems as it goes."""

from collections.abc import Callable
from pathlib import Path

import numpy as np
import soundfile as sf

from vt_ml.errors import UsageError
from vt_ml.separate import chunking
from vt_ml.separate.backends import Backend

VOICE = "voice.wav"
AMBIENCE = "ambience.wav"


def separate_file(
    input_path: Path,
    output_dir: Path,
    backend: Backend,
    on_progress: Callable[[int, int], None],
) -> tuple[Path, Path]:
    """Writes `voice.wav` and `ambience.wav` to `output_dir` and returns their paths.

    Removes both if anything goes wrong, including SIGTERM, so a failed run leaves nothing behind.
    Progress counts one unit per channel per chunk.
    """
    voice_path = output_dir / VOICE
    ambience_path = output_dir / AMBIENCE
    try:
        with sf.SoundFile(input_path) as source:
            _separate(source, voice_path, ambience_path, backend, on_progress)
    except BaseException:
        voice_path.unlink(missing_ok=True)
        ambience_path.unlink(missing_ok=True)
        raise
    return voice_path, ambience_path


def _separate(
    source: sf.SoundFile,
    voice_path: Path,
    ambience_path: Path,
    backend: Backend,
    on_progress: Callable[[int, int], None],
) -> None:
    if source.samplerate != backend.sample_rate:
        raise UsageError(
            f"input is {source.samplerate} Hz, but this backend needs {backend.sample_rate} Hz"
        )
    frames, channels = source.frames, source.channels
    chunks = chunking.plan(frames, backend.chunk_length, backend.overlap)
    total = len(chunks) * channels

    # RF64 lifts WAV's 4 GB limit, which float32 stereo passes after about 3 hours at 48 kHz.
    # Float samples keep stems that exceed full scale from clipping.
    def open_stem(path: Path) -> sf.SoundFile:
        return sf.SoundFile(
            path, "w", samplerate=source.samplerate, channels=channels, format="RF64", subtype="FLOAT"
        )

    with open_stem(voice_path) as voice_out, open_stem(ambience_path) as ambience_out:
        voice_blend = chunking.OverlapAdd(chunks, frames, backend.overlap, channels)
        ambience_blend = chunking.OverlapAdd(chunks, frames, backend.overlap, channels)
        for i, chunk in enumerate(chunks):
            source.seek(chunk.start)
            block = source.read(chunk.length, dtype="float32", always_2d=True)
            voice = np.empty_like(block)
            ambience = np.empty_like(block)

            # Mono models get one channel at a time, so stereo keeps its image.
            for c in range(channels):
                voice[:, c], ambience[:, c] = backend.separate(block[:, c], i)
                on_progress(i * channels + c + 1, total)
            voice_out.write(voice_blend.add(voice))
            ambience_out.write(ambience_blend.add(ambience))
