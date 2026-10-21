"""What a subcommand provides to the entry point."""

import argparse
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Protocol


@dataclass(frozen=True)
class Finished:
    """A subcommand's result fields, plus the device it ran on and its peak VRAM."""

    result: Mapping[str, object]
    device: str
    peak_vram_mb: float | None


class Subcommand(Protocol):
    def configure(self, parser: argparse.ArgumentParser) -> None:
        """Adds the subcommand's arguments."""

    def run(
        self,
        args: argparse.Namespace,
        report: Callable[[str], None],
        progress: Callable[[int, int], None],
    ) -> Finished:
        """Does the subcommand's work. Raises `UsageError` for failures the user can fix."""
