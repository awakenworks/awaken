#!/usr/bin/env python3
"""Real process tests for the shared CI deadline owner."""

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
import _strict_unittest as strict_unittest


# Actual execution subjects are the input authority; temporary-directory walks
# below inspect only this test's owned fixtures, never other repository paths.
INPUT_GLOBS: tuple[str, ...] = (
    "scripts/ci/deadline.py",
    "scripts/ci/_deadline.sh",
    "scripts/ci/test_deadline.py",
)
ROOT = Path(__file__).resolve().parents[2]
DEADLINE = ROOT / INPUT_GLOBS[0]
HELPER = ROOT / INPUT_GLOBS[1]


class DeadlineTests(unittest.TestCase):
    def cargo_fixture(self, root, command, *, status=0, delay=0.4, arguments=()):
        # This fixture owns only admission and Cargo's documented argv protocol;
        # it delegates execution/cleanup to the actual shared deadline owner.
        cargo = root / "cargo"
        cargo.write_text("""#!/usr/bin/env bash
if [ -z "${FIXTURE_ADMITTED:-}" ]; then
    sleep "$FIXTURE_ADMISSION_DELAY"
    [ "$FIXTURE_ADMISSION_STATUS" = 0 ] || exit "$FIXTURE_ADMISSION_STATUS"
    export FIXTURE_ADMITTED=1
fi
case "$1" in
    awaken-ci-deadline) entry=$(command -v "cargo-$1"); exec "$entry" "$@" ;;
    fixture) shift; exec "$@" ;;
    *) exit 2 ;;
esac
""")
        cargo.chmod(0o755)
        environment = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
                           TMPDIR=str(root), FIXTURE_ADMISSION_DELAY=str(delay),
                           FIXTURE_ADMISSION_STATUS=str(status))
        environment.pop("FIXTURE_ADMITTED", None)
        result = subprocess.run(
            ["bash", "-c", 'source "$1"; shift; run_with_deadline "$@"',
             "deadline-fixture", str(HELPER), "0.25", "cargo", "fixture",
             *command, *arguments],
            env=environment, capture_output=True, text=True, timeout=8,
        )
        self.assertEqual(list(root.glob("awaken-cargo-deadline.*")), [],
                         "the invocation must retire its own temporary entry")
        return result

    def test_cargo_admission_wait_does_not_spend_execution_budget(self):
        # A1/A2: C1 admission waits longer than the execution budget; C2 it
        # succeeds; C3 the real command finishes with zero/nonzero status.
        # Effects: retain C3's exact status/output and literal argv; admission
        # waiting must not become a proof timeout or execute shell metacharacters.
        # Shell builtins keep unrelated Python cold-start latency outside this
        # subsecond decision table. The actual execution budget stays 0.25s,
        # and admission still takes longer; an outer timer cannot pass A1/A2.
        for status in (0, 23):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                arguments = ("with spaces", "'quotes'", "$(touch unexpected);`literal`")
                result = self.cargo_fixture(
                    root, ["bash", "-c", f"printf '%s\\0' \"$@\"; exit {status}", "command"],
                    arguments=arguments,
                )
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(result.stdout.split("\0")[:-1], list(arguments))

    def test_cargo_admission_refusal_preserves_status_without_execution(self):
        # A3: C1 queued admission; C2 preflight refuses with 75. Effects: return
        # 75, start no command, and retire only this invocation's temporary entry.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            marker = root / "executed"
            result = self.cargo_fixture(
                root, ["bash", "-c", 'touch "$1"', "command"],
                status=75, arguments=(str(marker),),
            )
            self.assertEqual(result.returncode, 75, result.stderr)
            self.assertFalse(marker.exists())

    def test_cargo_execution_still_times_out_after_admission(self):
        # A4: C1 admission succeeds; C2 execution exceeds its unchanged budget.
        # Effects: actual execution emits readiness, then returns 124. D1-D4
        # below own the same run()/process-group cleanup's full TERM matrix.
        with tempfile.TemporaryDirectory() as tmp:
            result = self.cargo_fixture(
                Path(tmp), ["bash", "-c", "printf 'ready\\n'; exec sleep 100"],
            )
            self.assertEqual(result.returncode, 124, result.stderr)
            self.assertEqual(result.stdout, "ready\n")

    def test_cargo_protocol_rejects_missing_or_mismatched_name(self):
        # A5: the external entry lacks Cargo's injected name or carries a
        # different one => reject with 2 before starting any requested command.
        # A1/A2 above own the matching-name/unchanged-argv positive rules.
        with tempfile.TemporaryDirectory() as tmp:
            entry = Path(tmp) / "cargo-fixture-deadline"
            entry.symlink_to(DEADLINE)
            marker = Path(tmp) / "executed"
            for arguments in ([], ["wrong-name", "1", "--", sys.executable, "-c",
                                  "from pathlib import Path; import sys; Path(sys.argv[1]).touch()",
                                  str(marker)]):
                with self.subTest(arguments=arguments):
                    result = subprocess.run([sys.executable, str(entry), *arguments],
                                            capture_output=True, text=True, timeout=3)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertFalse(marker.exists())

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
        # status and bytes in direct/Python and non-Cargo/shell dispatch; ordinary
        # completion must not become a timeout when admission is inapplicable.
        for status in (0, 23):
            command = [sys.executable, "-c", f"print('complete'); raise SystemExit({status})"]
            entries = (
                [sys.executable, str(DEADLINE), "2", "--", *command],
                ["bash", "-c", 'source "$1"; shift; run_with_deadline "$@"',
                 "deadline-fixture", str(HELPER), "2", *command],
            )
            for entry in entries:
                with self.subTest(status=status, entry=entry[0]):
                    result = subprocess.run(entry, capture_output=True, text=True, timeout=3)
                    self.assertEqual(result.returncode, status, result.stderr)
                    self.assertEqual(result.stdout, "complete\n")

    def test_external_signals_use_shell_status_and_name_the_signal(self):
        # C1 child exits normally; C2 child dies from TERM or KILL before the
        # deadline; C3 deadline expires. E1 exact ordinary status (D5 above),
        # E2 128 + signal with diagnostic, E3 124 and group cleanup (D1-D4).
        # Decision rules D6=TERM -> 143, D7=KILL -> 137; neither may masquerade
        # as an ordinary 241/247 exit or as the deadline's 124.
        for number in (signal.SIGTERM, signal.SIGKILL):
            with self.subTest(signal=number):
                # Exercise an actual self-signal without a second interpreter's
                # cold start competing with this signal-specific deadline.
                result = subprocess.run(
                    [sys.executable, str(DEADLINE), "2", "--", "bash",
                     "-c", f"kill -{int(number)} $$"],
                    capture_output=True, text=True, timeout=3,
                )
                self.assertEqual(result.returncode, 128 + number)
                self.assertIn(f"command terminated by signal {number}", result.stderr)


if __name__ == "__main__":
    strict_unittest.main()
