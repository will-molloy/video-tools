"""Meta's SAM Audio, a generative separator prompted with text (https://arxiv.org/abs/2512.18099).

SAM Audio encodes the mix with a DAC-VAE codec, then a diffusion transformer generates the latents
of two stems, the prompted target and the residual, by integrating an ODE from noise. The codec
decodes both. Voice is the target for the prompt "speech" and ambience is the residual. Both are
generated, so unlike a mask-based model they need not sum back to the mix.

"Lite" mode loads only what text prompting uses. The span predictor and rerankers are left out of
the config, and the vision encoder is replaced by a stub that reports its width. The transformer
and text encoder run in bf16; the codec stays fp32, since its decoder writes the final samples.
"""

import json
import sys
import types
from pathlib import Path

import numpy as np
import torch

from vt_ml.device import peak_vram_mb, resolve
from vt_ml.errors import UsageError

REPO = "facebook/sam-audio-large"
PROMPT = "speech"


def _stub_xformers() -> None:
    """Satisfies perception-models' module-level `from xformers.ops import AttentionBias, fmha`.

    SAM Audio imports perception-models' audio encoder, whose transformer module imports xformers
    for one optional attention backend. SAM Audio's audio path runs `F.scaled_dot_product_attention`
    instead, and xformers has no ROCm kernels for RDNA4. pyproject.toml excludes it on every GPU,
    since nothing needs it, and this stub fails loudly if anything does call it.
    """

    class _Unavailable:
        def __getattr__(self, name: str):
            raise RuntimeError(f"xformers.ops.fmha.{name} was called, but xformers is stubbed out")

    ops = types.ModuleType("xformers.ops")
    ops.AttentionBias = type("AttentionBias", (), {})
    ops.fmha = _Unavailable()
    package = types.ModuleType("xformers")
    package.ops = ops
    sys.modules.setdefault("xformers", package)
    sys.modules.setdefault("xformers.ops", ops)


_stub_xformers()

from huggingface_hub import get_token, snapshot_download  # noqa: E402
from huggingface_hub.errors import GatedRepoError  # noqa: E402
from sam_audio import SAMAudio, SAMAudioProcessor  # noqa: E402
from sam_audio.model.config import SAMAudioConfig  # noqa: E402


class _NoVision(torch.nn.Module):
    """Stands in for the vision encoder. With no video, SAM Audio reads only its width."""

    def __init__(self, dim: int):
        super().__init__()
        self.dim = dim


class _Fp32:
    """Runs a callable with autocast off and float32 inputs, so the codec keeps full precision."""

    def __init__(self, fn):
        self._fn = fn

    def __call__(self, x: torch.Tensor) -> torch.Tensor:
        with torch.autocast(x.device.type, enabled=False):
            return self._fn(x.float())


class SamBackend:
    sample_rate = 48_000
    chunk_length = 10 * 48_000
    overlap = 1 * 48_000

    def __init__(self, device: str, log):
        self._torch_device, self.device = resolve(device)
        log(f"Loading {REPO}")
        checkpoint = _download()
        self._model, config = _load(checkpoint)
        self._model.to(self._torch_device).eval()
        self._processor = SAMAudioProcessor(
            audio_hop_length=config.audio_codec.hop_length,
            audio_sampling_rate=config.audio_codec.sample_rate,
        )
        if self._processor.audio_sampling_rate != self.sample_rate:
            raise UsageError(
                f"{REPO} runs at {self._processor.audio_sampling_rate} Hz, "
                f"but this backend expects {self.sample_rate} Hz"
            )

    def separate(self, mono: np.ndarray, chunk_index: int) -> tuple[np.ndarray, np.ndarray]:
        n = len(mono)
        # The processor expects (channels, samples) and averages the channels.
        audio = torch.from_numpy(mono)[None]
        batch = self._processor(descriptions=[PROMPT], audios=[audio]).to(self._torch_device)

        # The ODE starts from `randn_like` noise. Seeding per chunk makes reruns, and so the
        # cache, reproducible.
        torch.manual_seed(chunk_index)
        with torch.autocast(self._torch_device.type, dtype=torch.bfloat16):
            result = self._model.separate(batch, predict_spans=False, reranking_candidates=1)
        voice = result.target[0][:n].float().cpu().numpy()
        ambience = result.residual[0][:n].float().cpu().numpy()
        return voice, ambience

    def peak_vram_mb(self) -> float | None:
        return peak_vram_mb(self._torch_device)


def _download() -> Path:
    try:
        return Path(snapshot_download(REPO))
    except GatedRepoError as e:
        url = f"https://huggingface.co/{REPO}"
        if get_token() is None:
            raise UsageError(
                f"{REPO} is gated. Request access at {url}, then log in with `uvx hf auth login`."
            ) from e
        raise UsageError(
            f"your Hugging Face account can't access {REPO} yet. Request access at {url}; "
            "Meta approves requests manually."
        ) from e


def _load(checkpoint: Path) -> tuple[SAMAudio, SAMAudioConfig]:
    """Builds the model in bf16 and fills it from a memory-mapped checkpoint.

    `SAMAudio.from_pretrained` builds the model in fp32 and loads the whole state dict into RAM,
    which for the large checkpoint means about 30 GB. Building in bf16 halves the model, and the
    memory map lets the OS page the checkpoint in and out instead of holding it.
    """
    raw = json.loads((checkpoint / "config.json").read_text())
    raw.update(span_predictor=None, visual_ranker=None, text_ranker=None)
    config = SAMAudioConfig(**raw)

    default_dtype = torch.get_default_dtype()
    torch.set_default_dtype(torch.bfloat16)
    try:
        model = SAMAudio(config)
    finally:
        torch.set_default_dtype(default_dtype)

    state = torch.load(
        checkpoint / "checkpoint.pt", map_location="cpu", mmap=True, weights_only=True
    )
    model.load_state_dict(state)

    model.vision_encoder = _NoVision(model.vision_encoder.dim)
    codec = model.audio_codec
    codec.float()
    prefix = "audio_codec."
    codec.load_state_dict(
        {k.removeprefix(prefix): v for k, v in state.items() if k.startswith(prefix)}
    )
    codec.forward = _Fp32(codec.forward)
    codec.decode = _Fp32(codec.decode)
    return model, config
