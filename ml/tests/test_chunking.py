import numpy as np
import pytest

from vt_ml.separate import chunking
from vt_ml.separate.chunking import Chunk, OverlapAdd

LENGTH = 1000
OVERLAP = 200


def blend(signal: np.ndarray, model) -> np.ndarray:
    """Runs `signal`, shaped (frames, channels), through `model(chunk_index, block)` chunk by
    chunk, the way the pipeline does."""
    frames = signal.shape[0]
    chunks = chunking.plan(frames, LENGTH, OVERLAP)
    ola = OverlapAdd(chunks, frames, OVERLAP, signal.shape[1])
    parts = [ola.add(model(i, signal[c.start : c.end])) for i, c in enumerate(chunks)]
    return np.concatenate(parts) if parts else np.zeros((0, signal.shape[1]), np.float32)


def test_plans_full_length_chunks_with_the_last_aligned_to_the_end():
    assert chunking.plan(2500, LENGTH, OVERLAP) == [
        Chunk(0, 1000),
        Chunk(800, 1000),
        Chunk(1500, 1000),
    ]


def test_plans_exact_hops_without_a_duplicate_last_chunk():
    assert chunking.plan(2600, LENGTH, OVERLAP) == [
        Chunk(0, 1000),
        Chunk(800, 1000),
        Chunk(1600, 1000),
    ]


def test_plans_one_short_chunk_for_short_input():
    assert chunking.plan(300, LENGTH, OVERLAP) == [Chunk(0, 300)]


def test_plans_nothing_for_empty_input():
    assert chunking.plan(0, LENGTH, OVERLAP) == []


def test_ramps_crossfade_to_exactly_one():
    up = chunking.ramp(OVERLAP)

    np.testing.assert_allclose(up + up[::-1], np.ones(OVERLAP), rtol=0, atol=1e-12)


@pytest.mark.parametrize("frames", [1, 999, 1000, 1001, 2500, 2600, 10_007])
def test_identity_model_reconstructs_the_input(frames):
    signal = np.random.default_rng(frames).standard_normal((frames, 2)).astype(np.float32)

    out = blend(signal, lambda i, block: block)

    np.testing.assert_allclose(out, signal, rtol=0, atol=1e-6)


def test_regular_overlaps_need_no_normalisation():
    # When chunks hop evenly, the windows alone sum to 1, so dividing by them changes nothing.
    frames = 2600
    chunks = chunking.plan(frames, LENGTH, OVERLAP)
    sums = np.zeros(frames)
    for c in chunks:
        sums[c.start : c.end] += chunking.window(c, frames, OVERLAP)

    np.testing.assert_allclose(sums, np.ones(frames), rtol=0, atol=1e-12)


def test_crossfades_seams_between_mismatched_chunks():
    # Each chunk comes back at a different gain, as a model's output level drifts between chunks.
    # Butting the chunks together would step by up to 0.5 at each seam; the crossfade spreads the
    # change over the overlap, so no sample moves by more than the steepest ramp slope allows.
    frames = 5000
    gains = [1.0, 0.5, 1.0, 0.5, 1.0, 0.5, 1.0]
    signal = np.ones((frames, 1), np.float32)

    out = blend(signal, lambda i, block: block * gains[i])[:, 0]

    max_step = 0.5 * np.pi / (2 * OVERLAP)
    assert np.abs(np.diff(out)).max() <= max_step * 1.01
    # The steps are gradual, not gone: the level still follows each chunk's gain.
    assert out[0] == 1.0
    assert out[1100] == 0.5


def test_streams_every_sample_exactly_once():
    frames = 10_007
    chunks = chunking.plan(frames, LENGTH, OVERLAP)
    ola = OverlapAdd(chunks, frames, OVERLAP, 1)

    sizes = [len(ola.add(np.zeros((c.length, 1), np.float32))) for c in chunks]

    # Each chunk releases the samples before the next chunk's start.
    assert sizes == [b.start - a.start for a, b in zip(chunks, chunks[1:])] + [
        frames - chunks[-1].start
    ]
    assert sum(sizes) == frames


def test_rejects_a_chunk_of_the_wrong_length():
    chunks = chunking.plan(2500, LENGTH, OVERLAP)
    ola = OverlapAdd(chunks, 2500, OVERLAP, 1)

    with pytest.raises(ValueError, match="chunk 0 has 999 samples, expected 1000"):
        ola.add(np.zeros((999, 1), np.float32))
