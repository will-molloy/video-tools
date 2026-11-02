"""`vt-ml transcribe`: the timestamped speech segments of a mono 16 kHz WAV."""

import argparse
from collections.abc import Callable
from pathlib import Path

import soundfile as sf

from vt_ml.errors import UsageError
from vt_ml.stage import Finished
from vt_ml.transcribe import backends
from vt_ml.transcribe.segment import Segment


class Transcribe:
    def configure(self, parser: argparse.ArgumentParser) -> None:
        parser.add_argument("--input", type=Path, required=True, help="Mono WAV at 16 kHz")
        parser.add_argument(
            "--backend", type=backends.Name, choices=list(backends.Name), required=True
        )
        parser.add_argument("--device", choices=["cpu", "cuda"], required=True)

    def run(
        self,
        args: argparse.Namespace,
        report: Callable[[str], None],
        progress: Callable[[int, int], None],
    ) -> Finished:
        # Checked before loading a model, which can take a minute.
        if not args.input.is_file():
            raise UsageError(f"input {args.input} does not exist")
        audio, rate = sf.read(args.input, dtype="float32", always_2d=True)
        if audio.shape[1] != 1:
            raise UsageError(f"input has {audio.shape[1]} channels, but transcription needs mono")
        if rate != backends.SAMPLE_RATE:
            raise UsageError(f"input is {rate} Hz, but transcription needs {backends.SAMPLE_RATE} Hz")

        backend = backends.load(args.backend, args.device, report)
        report(f"Transcribing on {backend.device}")
        segments = backend.transcribe(audio[:, 0], progress)
        return Finished(
            {"segments": [_json(s) for s in segments]},
            backend.device,
            backend.peak_vram_mb(),
        )


def _json(segment: Segment) -> dict[str, object]:
    return {"start": round(segment.start, 3), "end": round(segment.end, 3), "text": segment.text}


STAGE = Transcribe()
