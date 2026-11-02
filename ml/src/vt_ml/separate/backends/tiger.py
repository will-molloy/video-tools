"""TIGER-DnR, a mask-based dialogue/effects/music separator (https://arxiv.org/abs/2410.01469).

TIGER-DnR bundles three TIGER models, one per stem, and its `forward` runs all three. Each model
predicts three complex spectrogram masks that are forced to sum to 1, so its three outputs sum to
the mix. This backend runs only the dialogue model: voice is its dialogue output, and ambience is
its other two outputs (effects and music). Voice plus ambience then reconstructs the mix exactly,
and it costs a third of the full model.

Upstream's chunked inference folds channels into the batch but reads back only the first, which
turns stereo into a copy of the left channel. The pipeline feeds one channel at a time instead.
"""

import numpy as np
import torch

from vt_ml.device import peak_vram_mb, resolve
from vt_ml.separate.vendor.tiger.tiger_dnr import TIGERDNR

REPO = "JusperLee/TIGER-DnR"

# Output order of each of TIGER-DnR's models, per its `forward`.
DIALOGUE = 2

# The STFT window. Shorter input can't be transformed.
WINDOW = 2048


class TigerBackend:
    sample_rate = 44_100

    # Upstream infers in 12 s windows, the length it was trained on.
    chunk_length = 12 * 44_100
    overlap = 2 * 44_100

    def __init__(self, device: str, log):
        self._torch_device, self.device = resolve(device)
        log(f"Loading {REPO}")
        model = TIGERDNR.from_pretrained(REPO)
        self._model = model.dialog.to(self._torch_device).eval()

    def separate(self, mono: np.ndarray, chunk_index: int) -> tuple[np.ndarray, np.ndarray]:
        n = len(mono)
        padded = np.pad(mono, (0, max(0, WINDOW - n)))
        x = torch.from_numpy(padded).to(self._torch_device)[None, None, :]
        with torch.inference_mode():
            # Shaped (batch, stem, samples).
            out = self._model(x)[0, :, :n].float().cpu().numpy()
        voice = out[DIALOGUE]
        ambience = np.delete(out, DIALOGUE, axis=0).sum(axis=0)
        return voice, ambience

    def peak_vram_mb(self) -> float | None:
        return peak_vram_mb(self._torch_device)
