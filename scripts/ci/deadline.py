#!/usr/bin/env python3
"""Portable command deadline with whole-process-group cleanup."""

from __future__ import annotations

import os
import signal
import subprocess
import sys
import time
from pathlib import Path


TIMEOUT_EXIT = 124
KILL_GRACE_SECONDS = 5


def terminate_group(process: subprocess.Popen[bytes]) -> None:
    """Terminate the command and every descendant in its new process group."""
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    # The leader exiting is not evidence that its descendants have exited.
    # Reap it while retaining the same group through the complete TERM grace.
    until = time.monotonic() + KILL_GRACE_SECONDS
    while True:
        process.poll()
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            process.wait()
            return
        remaining = until - time.monotonic()
        if remaining <= 0:
            break
        time.sleep(min(remaining, 0.01))
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def run(seconds: float, command: list[str]) -> int:
    if seconds <= 0 or not command:
        return 2
    process = subprocess.Popen(command, start_new_session=True)
    try:
        return process.wait(timeout=seconds)
    except subprocess.TimeoutExpired:
        terminate_group(process)
        return TIMEOUT_EXIT
    except BaseException:
        terminate_group(process)
        raise


def main(argv: list[str]) -> int:
    # Cargo's external-command protocol inserts the subcommand name. The entry
    # is a temporary symlink to this owner, so admission completes before run()
    # starts its clock. Direct invocation retains exactly the same parser.
    entry = Path(argv[0]).name
    if entry.startswith("cargo-"):
        if argv[1:2] != [entry.removeprefix("cargo-")]:
            return 2
        argv = [argv[0], *argv[2:]]
    if len(argv) < 3:
        print(f"usage: {Path(argv[0]).name} SECONDS -- COMMAND [ARG ...]", file=sys.stderr)
        return 2
    try:
        seconds = float(argv[1])
    except ValueError:
        print("deadline must be a positive number", file=sys.stderr)
        return 2
    command = argv[3:] if argv[2] == "--" else argv[2:]
    return run(seconds, command)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
