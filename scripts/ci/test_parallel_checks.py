#!/usr/bin/env python3
"""Decision-table tests for the static queue and its real deadline worker."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import unittest
from _strict_unittest import main

ROOT = Path(__file__).resolve().parents[2]


@unittest.skipIf(os.name == "nt", "canonical Bash gate runs in Linux")
class ParallelTests(unittest.TestCase):
    def run_queue(self, root: Path, body: str, jobs: int = 2) -> subprocess.CompletedProcess[str]:
        script = f'''set -u
source {shlex.quote(str(ROOT / 'scripts/ci/_parallel_checks.sh'))}
parallel_checks_init {jobs} {shlex.quote(str(root / 'evidence'))} || exit $?
parallel_checks_result() {{ printf '%s|%s\\n' "$1" "$2" >> {shlex.quote(str(root / 'results'))}; }}
{body}
parallel_checks_drain
'''
        return subprocess.run(["bash", "-c", script], cwd=root, capture_output=True, text=True, timeout=20)

    def test_parallel_barrier_and_complete_failure_aggregation(self) -> None:
        # Q1 two independently submitted checks must both reach the barrier;
        # Q2 a later failed job retains its exact exit code; Q3 all jobs drain.
        # A sequential replacement cannot satisfy the first positive control.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "barrier.py").write_text('''import pathlib,sys,time
p=pathlib.Path(sys.argv[1]); p.write_text('ready')
other=pathlib.Path(sys.argv[2]); until=time.monotonic()+5
# awaken-wait: both actual child processes acknowledge the shared barrier.
while not other.exists() and time.monotonic()<until: time.sleep(0.01)
assert other.exists(), 'both jobs must run concurrently'
''')
            result = self.run_queue(root, """parallel_checks_submit first python3 barrier.py first second
parallel_checks_submit second python3 barrier.py second first
parallel_checks_submit failure sh -c 'exit 23'
""")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertCountEqual((root / "results").read_text().splitlines(), ["first|0", "second|0", "failure|23"])
            self.assertTrue(all((root / "evidence" / f"{index}.json").is_file() for index in range(3)))

    def test_finished_worker_frees_capacity_while_oldest_is_still_running(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "wait_for_third.py").write_text('''import pathlib,time
until=time.monotonic()+5
# awaken-wait: third is the actual next admitted worker's completion signal.
while not pathlib.Path('third').exists() and time.monotonic()<until: time.sleep(0.01)
assert pathlib.Path('third').exists(), 'a completed second worker must free its slot'
''')
            result = self.run_queue(root, """parallel_checks_submit oldest python3 wait_for_third.py
parallel_checks_submit second true
parallel_checks_submit third touch third
""")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertCountEqual((root / "results").read_text().splitlines(), ["oldest|0", "second|0", "third|0"])

    def test_missing_or_corrupt_receipt_cannot_become_green(self) -> None:
        for replacement in ("rm evidence/0.json", "printf '{}' > evidence/0.json"):
            with self.subTest(replacement=replacement), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                result = self.run_queue(root, f"parallel_checks_submit observed true\nwait \"${{parallel_pids[0]}}\"\n{replacement}\n")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((root / "results").read_text().strip(), "observed|1")

    def test_capacity_is_not_silently_unbounded(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            # With one slot, submit(second) must observe and retire first before
            # second can run. The child observes the real parent callback fact.
            result = self.run_queue(root, """parallel_checks_submit first true
parallel_checks_submit second sh -c 'test -s results'
test -s results || exit 55
""", jobs=1)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((root / "results").read_text().splitlines(), ["first|0", "second|0"])
        for jobs in (0, 33):
            with tempfile.TemporaryDirectory() as temporary:
                self.assertEqual(self.run_queue(Path(temporary), "", jobs=jobs).returncode, 2)

    def test_worker_timeout_and_signal_are_failures_with_child_cleanup(self) -> None:
        for signal_worker in (False, True):
            with self.subTest(signal_worker=signal_worker), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                marker = root / "pid"
                receipt = root / "receipt.json"
                child = "import os,pathlib,time,sys;pathlib.Path(sys.argv[1]).write_text(str(os.getpid()));time.sleep(120)"
                process = subprocess.Popen([sys.executable, str(ROOT / "scripts/ci/parallel_check.py"), "--receipt", str(receipt), "--timeout", "1", "--", sys.executable, "-c", child, str(marker)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                until = time.monotonic() + 5
                # awaken-wait: the actual child PID exists before cancellation.
                while not marker.exists() and process.poll() is None and time.monotonic() < until:
                    time.sleep(0.01)
                self.assertTrue(marker.exists())
                if signal_worker:
                    process.terminate()
                output = process.communicate(timeout=10)
                self.assertEqual(process.returncode, 143 if signal_worker else 124, output)
                self.assertNotEqual(json.loads(receipt.read_text())["status"], 0)
                with self.assertRaises(ProcessLookupError):
                    os.kill(int(marker.read_text()), 0)


if __name__ == "__main__":
    main()
