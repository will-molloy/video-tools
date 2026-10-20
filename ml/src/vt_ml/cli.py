"""Entry point `vt` spawns as `vt-ml <stage> ...`. See `ml/README.md` for the contract."""

import argparse
import logging
import signal
import sys
import time
import traceback
from collections.abc import Mapping
from typing import NoReturn

from vt_ml.errors import Terminated, UsageError
from vt_ml.protocol import EXIT_ERROR, EXIT_OK, EXIT_TERMINATED, Protocol, Stats, claim_stdout
from vt_ml.separate import STAGE as SEPARATE
from vt_ml.stage import Stage
from vt_ml.transcribe import STAGE as TRANSCRIBE

log = logging.getLogger("vt_ml")

STAGES: dict[str, Stage] = {"separate": SEPARATE, "transcribe": TRANSCRIBE}


class _Parser(argparse.ArgumentParser):
    """Raises instead of printing usage and exiting, so bad arguments become an `error` message."""

    def error(self, message: str) -> NoReturn:
        raise UsageError(message)


def _parser(stages: Mapping[str, Stage]) -> argparse.ArgumentParser:
    parser = _Parser(prog="vt-ml", description=__doc__)
    subparsers = parser.add_subparsers(dest="stage", required=True, parser_class=_Parser)
    for name, stage in stages.items():
        stage.configure(subparsers.add_parser(name))
    return parser


def _on_sigterm(signum, frame) -> NoReturn:
    # uv forwards SIGTERM to its child and `vt` signals the whole group, so a second copy can
    # arrive mid-cleanup. Ignore it.
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    raise Terminated


def main(argv: list[str] | None = None, stages: Mapping[str, Stage] = STAGES) -> NoReturn:
    protocol = claim_stdout()
    logging.basicConfig(stream=sys.stderr, level=logging.INFO, format="%(name)s: %(message)s")
    signal.signal(signal.SIGTERM, _on_sigterm)
    sys.exit(run(protocol, argv, stages))


def run(protocol: Protocol, argv: list[str] | None, stages: Mapping[str, Stage]) -> int:
    try:
        args = _parser(stages).parse_args(argv)
        start = time.monotonic()

        def report(message: str) -> None:
            log.info(message)
            protocol.log(message)

        finished = stages[args.stage].run(args, report, protocol.progress)
        protocol.done(
            finished.result,
            Stats(finished.device, time.monotonic() - start, finished.peak_vram_mb),
        )
        return EXIT_OK
    except Terminated:
        log.info("terminated")
        return EXIT_TERMINATED
    except UsageError as e:
        protocol.error(str(e))
        return EXIT_ERROR
    except Exception as e:
        traceback.print_exc()
        protocol.error(f"{type(e).__name__}: {e}")
        return EXIT_ERROR
