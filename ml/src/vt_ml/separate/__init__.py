"""`vt-ml separate`: split a WAV into voice and ambience stems."""

import argparse
from collections.abc import Callable
from pathlib import Path

from vt_ml.errors import UsageError
from vt_ml.separate import backends, pipeline
from vt_ml.stage import Finished


class Separate:
    def configure(self, parser: argparse.ArgumentParser) -> None:
        parser.add_argument("--input", type=Path, required=True, help="WAV at the backend's rate")
        parser.add_argument("--output-dir", type=Path, required=True)
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
        if not args.output_dir.is_dir():
            raise UsageError(f"output directory {args.output_dir} does not exist")

        backend = backends.load(args.backend, args.device, report)
        report(f"Separating on {backend.device}")
        voice, ambience = pipeline.separate_file(args.input, args.output_dir, backend, progress)
        return Finished(
            {"voice": str(voice), "ambience": str(ambience)},
            backend.device,
            backend.peak_vram_mb(),
        )


STAGE = Separate()
