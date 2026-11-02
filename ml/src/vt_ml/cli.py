"""Entry point `vt` spawns as `vt-ml <subcommand> ...`. See `ml/README.md` for the contract."""

import _thread
import argparse
import logging
import os
import signal
import sys
import threading
import time
import traceback
from collections.abc import Mapping
from typing import NoReturn

from vt_ml.errors import Terminated, UsageError
from vt_ml.protocol import EXIT_ERROR, EXIT_OK, EXIT_TERMINATED, Protocol, Stats, claim_stdout
from vt_ml.separate import SUBCOMMAND as SEPARATE
from vt_ml.subcommand import Subcommand
from vt_ml.transcribe import SUBCOMMAND as TRANSCRIBE

log = logging.getLogger("vt_ml")

SUBCOMMANDS: dict[str, Subcommand] = {"separate": SEPARATE, "transcribe": TRANSCRIBE}


class _Parser(argparse.ArgumentParser):
    """Raises instead of printing usage and exiting, so bad arguments become an `error` message."""

    def error(self, message: str) -> NoReturn:
        raise UsageError(message)


def _parser(subcommands: Mapping[str, Subcommand]) -> argparse.ArgumentParser:
    parser = _Parser(prog="vt-ml", description=__doc__)
    subparsers = parser.add_subparsers(dest="subcommand", required=True, parser_class=_Parser)
    for name, subcommand in subcommands.items():
        subcommand.configure(subparsers.add_parser(name))
    return parser


def _on_sigterm(signum, frame) -> NoReturn:
    # Closing stdin and a SIGTERM can both arrive, so ignore the second mid-cleanup.
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    raise Terminated


def _terminate_when_stdin_closes() -> None:
    """Raises `Terminated` in the main thread once stdin reaches EOF.

    Closing stdin is how `vt` cancels on every platform, since Windows has no SIGTERM. The pipe
    also closes when `vt` dies, so `vt-ml` doesn't outlive it.
    """

    def watch() -> None:
        # Reads the fd directly, because a daemon thread blocked on `sys.stdin`'s lock aborts
        # the interpreter at exit.
        while os.read(0, 4096):
            pass
        _thread.interrupt_main(signal.SIGTERM)

    threading.Thread(target=watch, name="stdin-eof", daemon=True).start()


def main(
    argv: list[str] | None = None, subcommands: Mapping[str, Subcommand] = SUBCOMMANDS
) -> NoReturn:
    protocol = claim_stdout()
    logging.basicConfig(stream=sys.stderr, level=logging.WARNING, format="%(name)s: %(message)s")
    log.setLevel(logging.INFO)
    signal.signal(signal.SIGTERM, _on_sigterm)
    _terminate_when_stdin_closes()
    sys.exit(run(protocol, argv, subcommands))


def run(protocol: Protocol, argv: list[str] | None, subcommands: Mapping[str, Subcommand]) -> int:
    try:
        args = _parser(subcommands).parse_args(argv)
        start = time.monotonic()

        def report(message: str) -> None:
            log.info(message)
            protocol.log(message)

        finished = subcommands[args.subcommand].run(args, report, protocol.progress)
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
