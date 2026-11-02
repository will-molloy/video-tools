"""Runs `vt-ml` in a subprocess as `vt` does, with stdin open until it exits, since closing
stdin cancels it."""

import os
import subprocess


def run(args: list[str]) -> subprocess.CompletedProcess[str]:
    read, write = os.pipe()
    try:
        return subprocess.run(args, stdin=read, capture_output=True, text=True, timeout=60)
    finally:
        os.close(read)
        os.close(write)


def spawn(args: list[str], env: dict[str, str] | None = None) -> subprocess.Popen[str]:
    """Starts `args` with piped stdin and stdout. Closing `stdin` cancels it."""
    return subprocess.Popen(
        args,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env=env,
    )
