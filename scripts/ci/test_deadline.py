#!/usr/bin/env python3
"""Real process tests for the shared CI deadline owner."""

import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest


DEADLINE = Path(__file__).with_name("deadline.py").resolve()


class DeadlineTests(unittest.TestCase):
    def test_timeout_reaps_group_even_when_leader_exits_first(self):
        # Causes: C1 leader handles/ignores TERM; C2 child handles/ignores TERM;
        # C3 command exceeds its deadline. Effects: E1 result remains 124;
        # E2 every inherited output descriptor closes before return; E3 no live
        # child survives. Decision table D1-D4: C3 x all C1/C2 pairs => E1-E3.
        # A child inherits stdout so communicate cannot complete while it lives.
        # The readiness line precedes the deadline: this is actual group cleanup,
        # not a parent-only sleep or a mocked killpg. Final cleanup addresses only
        # the process group created by this fixture, including on a failed test.
        with tempfile.TemporaryDirectory(prefix="awaken-deadline-test-") as tmp:
            for leader_ignores in (False, True):
                for child_ignores in (False, True):
                    with self.subTest(leader=leader_ignores, child=child_ignores):
                        coordinate = Path(tmp) / "group"
                        coordinate.unlink(missing_ok=True)
                        code = """
import os, signal, sys, time
from pathlib import Path
Path(sys.argv[1]).write_text(str(os.getpid()))
ready_read, ready_write = os.pipe()
child = os.fork()
if child == 0:
    os.close(ready_read)
    signal.signal(signal.SIGTERM, signal.SIG_IGN if sys.argv[3] == '1' else signal.SIG_DFL)
    os.write(ready_write, b'R')
    os.close(ready_write)
else:
    os.close(ready_write)
    signal.signal(signal.SIGTERM, signal.SIG_IGN if sys.argv[2] == '1' else signal.SIG_DFL)
    assert os.read(ready_read, 1) == b'R'
    os.close(ready_read)
    print('ready', flush=True)
while True:
    time.sleep(1)
"""
                        # Shorten only cleanup grace in this isolated Python
                        # invocation; production's five-second grace is unchanged.
                        driver = """
import importlib.util, sys
spec = importlib.util.spec_from_file_location('deadline', sys.argv[1])
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
m.KILL_GRACE_SECONDS = 0.15
raise SystemExit(m.run(0.5, sys.argv[2:]))
"""
                        try:
                            result = subprocess.run(
                                [sys.executable, "-c", driver, str(DEADLINE),
                                 sys.executable, "-c", code, str(coordinate),
                                 str(int(leader_ignores)), str(int(child_ignores))],
                                capture_output=True, text=True, timeout=3,
                            )
                            self.assertEqual(result.returncode, 124, result.stderr)
                            self.assertEqual(result.stdout.strip(), "ready")
                        finally:
                            if coordinate.exists():
                                try:
                                    os.killpg(int(coordinate.read_text()), signal.SIGKILL)
                                except ProcessLookupError:
                                    pass

    def test_success_and_failure_preserve_command_status(self):
        # D5: command returns before deadline, with zero/nonzero status => exact
        # status and bytes; ordinary completion must not become a timeout.
        for status in (0, 23):
            with self.subTest(status=status):
                result = subprocess.run(
                    [sys.executable, str(DEADLINE), "2", "--", sys.executable,
                     "-c", f"print('complete'); raise SystemExit({status})"],
                    capture_output=True, text=True, timeout=3,
                )
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(result.stdout, "complete\n")


if __name__ == "__main__":
    unittest.main()
