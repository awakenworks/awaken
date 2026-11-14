"""Retire a fixture's own process group, shared by the process tests.

Kept out of test_deadline.py so a test that borrows it does not inherit that
test's whole input closure (generate_hooks.py --script-tests follows imports),
which made script edits run the borrower.
"""

import os
from pathlib import Path
import signal


def cleanup_fixture_group(coordinate: Path, process=None, *, pipe_timeout=5) -> None:
    """Retire only this fixture's group, including a nested inherited group."""
    if coordinate.exists():
        leader = int(coordinate.read_text())
        try:
            group = os.getpgid(leader)
        except ProcessLookupError:
            group = leader
        if group == os.getpgrp():
            raise RuntimeError("fixture cleanup refuses its observer's process group")
        try:
            os.killpg(group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if process is not None:
        if process.poll() is None:
            process.kill()
        process.communicate(timeout=pipe_timeout)

