"""Runs the entry point with stub stages, in a subprocess like `vt` does, and checks the protocol
it speaks."""

import json
import signal
import subprocess
import sys
from pathlib import Path

# Defines stub stages, then runs the entry point with them and the given arguments.
SCRIPT = """
import sys, time
from vt_ml import cli
from vt_ml.errors import UsageError
from vt_ml.stage import Finished

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
            time.sleep(60)
        finally:
            import os
            os.remove(args.partial)

cli.main(sys.argv[1:], {"echo": Echo(), "sleep": Sleep()})
"""


def command(*args: str) -> list[str]:
    return [sys.executable, "-c", SCRIPT, *args]


def run(*args: str):
    result = subprocess.run(command(*args), capture_output=True, text=True, timeout=60)
    # Every stdout line must parse, or `vt` would reject the stream.
    messages = [json.loads(line) for line in result.stdout.splitlines()]
    return result.returncode, messages, result.stderr


def test_runs_a_stage_and_reports_its_result():
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


def test_reports_an_unknown_stage():
    code, messages, _ = run("separate")

    assert code == 2
    assert messages == [
        {
            "type": "error",
            "message": "argument stage: invalid choice: 'separate' (choose from echo, sleep)",
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

    result = subprocess.run([sys.executable, "-c", script], capture_output=True, text=True)

    assert result.stdout == '{"type": "log", "message": "hello"}\n'
    assert result.stderr == "from python\nfrom c\n"


def test_stops_on_sigterm_and_lets_the_stage_clean_up(tmp_path: Path):
    partial = tmp_path / "partial"
    child = subprocess.Popen(
        command("sleep", "--partial", str(partial)),
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )

    # Wait until it's mid-run, with its partial output written.
    assert json.loads(child.stdout.readline())["type"] == "progress"
    assert partial.exists()
    child.send_signal(signal.SIGTERM)

    assert child.wait(timeout=10) == 143
    assert child.stdout.read() == ""
    assert not partial.exists()
