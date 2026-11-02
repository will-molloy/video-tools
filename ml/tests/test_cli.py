"""Runs the entry point with stub subcommands, in a subprocess like `vt` does, and checks the
protocol it speaks."""

import json
import signal
import subprocess
import sys
from pathlib import Path

import pytest

import child

# Defines stub subcommands, then runs the entry point with them and the given arguments.
SCRIPT = """
import sys, time
from vt_ml import cli
from vt_ml.errors import UsageError
from vt_ml.subcommand import Finished

class Echo:
    def configure(self, parser):
        parser.add_argument("--word", required=True)

    def run(self, args, report, progress):
        report("Echoing")
        progress(1, 1)
        if args.word == "bad":
            raise UsageError("bad is not a word")
        if args.word == "crash":
            raise RuntimeError("boom")
        return Finished({"word": args.word}, "cpu", None)

class Sleep:
    def configure(self, parser):
        parser.add_argument("--partial", required=True)

    def run(self, args, report, progress):
        open(args.partial, "w").close()
        try:
            progress(0, 1)
            # Short sleeps, since Windows only interrupts a sleep for SIGINT.
            for _ in range(600):
                time.sleep(0.1)
        finally:
            import os
            os.remove(args.partial)

cli.main(sys.argv[1:], {"echo": Echo(), "sleep": Sleep()})
"""


def command(*args: str) -> list[str]:
    return [sys.executable, "-c", SCRIPT, *args]


def run(*args: str):
    result = child.run(command(*args))
    # Every stdout line must parse, or `vt` would reject the stream.
    messages = [json.loads(line) for line in result.stdout.splitlines()]
    return result.returncode, messages, result.stderr


def test_runs_a_subcommand_and_reports_its_result():
    code, messages, _ = run("echo", "--word", "hello")

    assert code == 0
    done = messages[-1]
    assert messages == [
        {"type": "log", "message": "Echoing"},
        {"type": "progress", "done": 1, "total": 1},
        {
            "type": "done",
            "word": "hello",
            "device": "cpu",
            "elapsed_s": done["elapsed_s"],
            "peak_vram_mb": None,
        },
    ]


def test_reports_a_usage_error_without_a_traceback():
    code, messages, stderr = run("echo", "--word", "bad")

    assert code == 2
    assert messages[-1] == {"type": "error", "message": "bad is not a word"}
    assert "Traceback" not in stderr


def test_reports_a_crash_with_its_type_and_a_traceback_on_stderr():
    code, messages, stderr = run("echo", "--word", "crash")

    assert code == 2
    assert messages[-1] == {"type": "error", "message": "RuntimeError: boom"}
    assert "Traceback" in stderr


def test_reports_bad_arguments():
    code, messages, _ = run("echo")

    assert code == 2
    assert messages == [{"type": "error", "message": "the following arguments are required: --word"}]


def test_reports_an_unknown_subcommand():
    code, messages, _ = run("separate")

    assert code == 2
    assert messages == [
        {
            "type": "error",
            "message": "argument subcommand: invalid choice: 'separate' (choose from echo, sleep)",
        }
    ]


def test_keeps_library_output_off_the_protocol_stream():
    script = (
        "import os\n"
        "from vt_ml.protocol import claim_stdout\n"
        "protocol = claim_stdout()\n"
        "print('from python')\n"
        "os.write(1, b'from c\\n')\n"
        "protocol.log('hello')\n"
    )

    result = subprocess.run([sys.executable, "-c", script], capture_output=True)

    assert result.stdout == b'{"type": "log", "message": "hello"}\n'
    assert result.stderr.decode().splitlines() == ["from python", "from c"]


def start_sleeping(partial: Path) -> subprocess.Popen[str]:
    """Starts the `sleep` stub and waits until it's mid-run, with its partial output written."""
    sleeping = child.spawn(command("sleep", "--partial", str(partial)))
    assert json.loads(sleeping.stdout.readline())["type"] == "progress"
    assert partial.exists()
    return sleeping


def test_stops_when_stdin_closes_and_lets_the_subcommand_clean_up(tmp_path: Path):
    partial = tmp_path / "partial"
    sleeping = start_sleeping(partial)

    sleeping.stdin.close()

    assert sleeping.wait(timeout=10) == 143
    assert sleeping.stdout.read() == ""
    assert not partial.exists()


@pytest.mark.skipif(sys.platform == "win32", reason="Windows has no SIGTERM")
def test_stops_on_sigterm_and_lets_the_subcommand_clean_up(tmp_path: Path):
    partial = tmp_path / "partial"
    sleeping = start_sleeping(partial)

    sleeping.send_signal(signal.SIGTERM)

    assert sleeping.wait(timeout=10) == 143
    assert sleeping.stdout.read() == ""
    assert not partial.exists()
