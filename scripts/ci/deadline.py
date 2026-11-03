#!/usr/bin/env python3
"""Portable command deadline with whole-process-group cleanup."""

from __future__ import annotations

import os
import math
import signal
import subprocess
import sys
import time
from pathlib import Path


TIMEOUT_EXIT = 124
KILL_GRACE_SECONDS = 5


class Cancelled(BaseException):
    def __init__(self, number: int):
        self.number = number


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


def run(seconds: float, command: list[str], *, stdout=None, stderr=None, environment=None) -> int:
    if not math.isfinite(seconds) or seconds <= 0 or not command:
        return 2
    stdio = {name: value for name, value in (('stdout', stdout), ('stderr', stderr)) if value is not None}
    if environment is not None:
        stdio['env'] = environment
    if os.name == "nt":
        from _windows_job import WindowsJob

        with WindowsJob() as job:
            process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()),
                                        "--windows-job-child", job.name, "--", *command],
                                       **stdio)
            try:
                return process.wait(timeout=seconds)
            except subprocess.TimeoutExpired:
                job.close()
                # A trampoline still starting Python may not have joined yet.
                # It cannot launch the command before joining the owned Job.
                if process.poll() is None:
                    process.kill()
                process.wait()
                return TIMEOUT_EXIT
            except BaseException:
                job.close()
                if process.poll() is None:
                    process.kill()
                process.wait()
                raise
    process = None
    pending_signal = None
    cleaning = False
    previous_handlers = {}

    def interrupted(number: int, _frame: object) -> None:
        nonlocal pending_signal
        if cleaning:
            return
        pending_signal = pending_signal or number
        # During Popen construction the child may exist before Python assigns
        # its handle. Defer cancellation until that exact handle is retained.
        if process is not None:
            raise Cancelled(pending_signal)

    def cleanup() -> None:
        nonlocal cleaning
        cleaning = True
        if process is not None:
            terminate_group(process)

    try:
        for number in (signal.SIGTERM, signal.SIGINT):
            previous_handlers[number] = signal.signal(number, interrupted)
        process = subprocess.Popen(command, start_new_session=True, **stdio)
        if pending_signal is not None:
            raise Cancelled(pending_signal)
        result = process.wait(timeout=seconds)
        if result < 0:
            # A child killed by a signal reports a negative Python return code.
            # Shell callers need the conventional 128 + signal status; otherwise
            # SystemExit(-15) wraps to 241 and hides the reason for failure.
            number = -result
            print(f"command terminated by signal {number}", file=sys.stderr)
            return 128 + number
        return result
    except subprocess.TimeoutExpired:
        cleanup()
        return TIMEOUT_EXIT
    except Cancelled as cancellation:
        cleanup()
        print(f"deadline cancelled by signal {cancellation.number}", file=sys.stderr)
        return 128 + cancellation.number
    except BaseException:
        cleanup()
        raise
    finally:
        for number, handler in previous_handlers.items():
            signal.signal(number, handler)


def main(argv: list[str]) -> int:
    if argv[1:2] == ["--windows-job-child"]:
        if os.name != "nt" or len(argv) < 5 or argv[3] != "--":
            return 2
        from _windows_job import attach_current_process

        attach_current_process(argv[2])
        command = argv[4:]
        if command[0].casefold() in {"bash", "bash.exe"}:
            # CreateProcess searches the current/system directories before PATH.
            # Resolve native Bash only after joining the owned Job; explicit
            # command paths retain the caller's authority.
            from _native_shell import bash_executable

            command = [bash_executable(), *command[1:]]
        return subprocess.call(command)
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
