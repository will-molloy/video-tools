"""Whisper large-v3-turbo through transformers, which runs on ROCm and CUDA alike.

`generate` runs Whisper's long-form algorithm on input over 30 s. It reads the audio in 30 s
windows, starts each at the last segment it finished, and retries a window at higher
temperatures when the result looks like a hallucination. Shorter input is padded with silence
to take the same path, since only long-form returns segments.

A segment's own start snaps back to its window's start when silence precedes the speech, so
segments are trimmed to their words' timestamps, which come from aligning each token with the
audio.

Word timestamps need the model's attentions, and long-form keeps every window's outputs until it
returns. The encoder's attentions alone are about 3 GB a window, so `_Whisper` drops the
attentions once a window's timestamps are extracted.
"""

from collections.abc import Callable

import numpy as np
import torch
from transformers import AutoProcessor, WhisperForConditionalGeneration

from vt_ml.device import peak_vram_mb, resolve
from vt_ml.transcribe.backends import SAMPLE_RATE
from vt_ml.transcribe.segment import Segment

REPO = "openai/whisper-large-v3-turbo"

# Just over Whisper's 30 s window, which is the shortest long-form input.
MIN_SAMPLES = 30 * SAMPLE_RATE + 1

# A mel frame covers 10 ms.
MS_PER_FRAME = 10

# OpenAI's defaults for detecting hallucinations, which transformers leaves off.
DECODING = {
    "condition_on_prev_tokens": False,
    "temperature": (0.0, 0.2, 0.4, 0.6, 0.8, 1.0),
    "compression_ratio_threshold": 1.35,
    "logprob_threshold": -1.0,
    "no_speech_threshold": 0.6,
}


class _Whisper(WhisperForConditionalGeneration):
    """Keeps only what long-form generation reads from a window: its tokens, scores and
    timestamps."""

    def _postprocess_outputs(self, seek_outputs, **kwargs):
        if isinstance(seek_outputs, torch.Tensor):
            return super()._postprocess_outputs(seek_outputs=seek_outputs, **kwargs)

        # Emptied before they're copied to the CPU.
        for key in ("encoder_attentions", "decoder_attentions"):
            if seek_outputs.get(key) is not None:
                seek_outputs[key] = ()
        sequences, outputs = super()._postprocess_outputs(seek_outputs=seek_outputs, **kwargs)

        # Read once, to extract the token timestamps.
        for output in outputs:
            output.pop("cross_attentions", None)
        return sequences, outputs


class WhisperBackend:
    def __init__(self, device: str, log: Callable[[str], None]):
        self._torch_device, self.device = resolve(device)
        log(f"Loading {REPO}")
        self._processor = AutoProcessor.from_pretrained(REPO)
        self._model = (
            _Whisper.from_pretrained(REPO, dtype=torch.float16)
            .to(self._torch_device)
            .eval()
        )

    def transcribe(
        self, audio: np.ndarray, progress: Callable[[int, int], None]
    ) -> list[Segment]:
        duration = len(audio) / SAMPLE_RATE
        padded = np.pad(audio, (0, max(0, MIN_SAMPLES - len(audio))))
        inputs = self._processor(
            padded,
            sampling_rate=SAMPLE_RATE,
            return_tensors="pt",
            truncation=False,
            padding="longest",
            return_attention_mask=True,
        ).to(self._torch_device, torch.float16)
        total_ms = len(audio) * 1000 // SAMPLE_RATE

        def monitor(p: torch.Tensor) -> None:
            progress(min(int(p[0, 0]) * MS_PER_FRAME, total_ms), total_ms)

        with torch.inference_mode():
            output = self._model.generate(
                **inputs,
                task="transcribe",
                return_timestamps=True,
                return_segments=True,
                return_token_timestamps=True,
                monitor_progress=monitor,
                **DECODING,
            )
        progress(total_ms, total_ms)

        # Whisper's special and timestamp tokens all sit after the text vocabulary.
        first_special = self._model.generation_config.eos_token_id
        segments = []
        for s in output["segments"][0]:
            tokens, times = s["tokens"].cpu(), s["token_timestamps"].cpu()
            words = tokens < first_special
            if not words.any():
                continue
            # A token's timestamp is when it starts, so the last word may run past the segment.
            start = float(times[words][0])
            end = min(max(float(s["end"]), float(times[words][-1])), duration)
            text = self._processor.decode(tokens[words]).strip()
            if text and start < end:
                segments.append(Segment(start, end, text))
        return segments

    def peak_vram_mb(self) -> float | None:
        return peak_vram_mb(self._torch_device)
