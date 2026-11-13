#!/usr/bin/env python3
"""Real process tests for the shared CI deadline owner."""

import io
import json
import os
from pathlib import Path
import signal
import socket
import shlex
import subprocess
import sys
import tempfile
import time
import unittest
import _strict_unittest as strict_unittest
import deadline
from _native_shell import bash_executable


# Actual execution subjects are the input authority; temporary-directory walks
# below inspect only this test's owned fixtures, never other repository paths.
INPUT_GLOBS: tuple[str, ...] = (
    "scripts/ci/deadline.py",
    "scripts/ci/_deadline.sh",
    "scripts/ci/test_deadline.py",
    "scripts/ci/_windows_job.py",
    "scripts/ci/_native_shell.py",
)
ROOT = Path(__file__).resolve().parents[2]
DEADLINE = ROOT / INPUT_GLOBS[0]
HELPER = ROOT / INPUT_GLOBS[1]

GROUP_CODE = """
import json, os, signal, sys, time
from pathlib import Path
def publish(path, value):
    pending = path.with_name(path.name + '.pending')
    pending.write_text(value)
    pending.replace(path)
publish(Path(sys.argv[1]), str(os.getpid()))
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
    publish(Path(sys.argv[1] + '.ready'), json.dumps({'leader':os.getpid(),'child':child}))
    print('ready', flush=True)
while True:
    time.sleep(1)
"""


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


@unittest.skipIf(os.name == "nt", "POSIX process groups and Cargo admission are owned by Linux/macOS")
class DeadlineTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux owned PID observation")
    def test_group_coordinates_publish_atomically_and_cleanup_failed_observers(self):
        # Causes C1 a readiness write is paused after its first byte; C2 the
        # write completes; C3 its observer then raises. R1 C1 => no published
        # readiness file. R2 C2 => one complete exact leader/child document.
        # R3 C3 => finally releases the actual owned group and inherited pipes.
        # The same GROUP_CODE producer is exercised; no retrying JSON reader
        # or relaxed process/deadline assertion can make R1 pass.
        import selectors
        driver = """
from pathlib import Path
import sys, time
original = Path.write_text
def controlled(path, value, *args, **kwargs):
    if '.ready' not in path.name:
        return original(path, value, *args, **kwargs)
    with path.open('w') as stream:
        stream.write(value[:1]); stream.flush()
        print('partial', flush=True)
        release = Path(sys.argv[1] + '.release')
        until = time.monotonic() + 10
        while not release.exists() and time.monotonic() < until:
            time.sleep(.01)
        assert release.exists(), 'observer did not release the controlled write'
        stream.write(value[1:])
    return len(value)
Path.write_text = controlled
exec(sys.argv[4])
"""
        with tempfile.TemporaryDirectory() as tmp:
            coordinate = Path(tmp) / "group"
            ready = Path(str(coordinate) + ".ready")
            members = {}
            process = subprocess.Popen(
                [sys.executable, "-c", driver, str(coordinate), "0", "0", GROUP_CODE],
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, start_new_session=True)
            with self.assertRaisesRegex(RuntimeError, "controlled observer failure"):
                try:
                    with selectors.DefaultSelector() as barrier:
                        barrier.register(process.stdout, selectors.EVENT_READ)
                        self.assertTrue(barrier.select(10), "R1 partial write barrier")
                        self.assertEqual(process.stdout.readline(), "partial\n")
                        self.assertFalse(ready.exists(), "R1 incomplete readiness must be invisible")
                        Path(str(coordinate) + ".release").write_text("continue")
                        self.assertTrue(barrier.select(10), "R2 complete write barrier")
                        self.assertEqual(process.stdout.readline(), "ready\n")
                    members = json.loads(ready.read_text())
                    self.assertEqual(members["leader"], process.pid, "R2 exact producer")
                    self.assertGreater(members["child"], 0, "R2 exact child")
                    raise RuntimeError("controlled observer failure")
                finally:
                    cleanup_fixture_group(coordinate, process)
            self.assertIsNotNone(process.poll(), "R3 producer reaped")
            for pid in members.values():
                stat = Path(f"/proc/{pid}/stat")
                try:
                    state = stat.read_text().split()[2]
                except (FileNotFoundError, ProcessLookupError):
                    state = None  # Reaping can retire /proc between lookup and read.
                self.assertIn(state, (None, "Z"), "R3 no surviving fixture process")

    def test_fixture_cleanup_refuses_the_observer_group_before_any_signal(self):
        # F1 coordinate names the observer itself => refuse before signaling.
        # A controlled signal boundary makes a missing guard fail without
        # killing the test runner; real owned/expired groups use R3/D1-D4/S1.
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            coordinate = Path(tmp) / "group"
            coordinate.write_text(str(os.getpid()))
            with patch.object(os, "killpg", side_effect=AssertionError("observer signaled")) as send:
                with self.assertRaisesRegex(RuntimeError, "observer's process group"):
                    cleanup_fixture_group(coordinate)
                send.assert_not_called()

    def group_command(self, coordinate, leader_ignores, child_ignores):
        return [sys.executable, "-c", GROUP_CODE, str(coordinate),
                str(int(leader_ignores)), str(int(child_ignores))]

    def cargo_fixture(self, root, command, *, status=0, delay=0.4, arguments=(), admit=False, ready=False,
                      shell_flags=(), redirected=False):
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
    awaken-ci-deadline)
        [ "$FIXTURE_READY" != 1 ] || printf 'ready\\n'
        entry=$(command -v "cargo-$1"); exec "$entry" "$@" ;;
    fixture) shift; exec "$@" ;;
    *) exit 2 ;;
esac
""")
        cargo.chmod(0o755)
        environment = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
                           TMPDIR=str(root), FIXTURE_ADMISSION_DELAY=str(delay),
                           FIXTURE_ADMISSION_STATUS=str(status), FIXTURE_READY=str(int(ready)))
        environment.pop("FIXTURE_ADMITTED", None)
        invocation = 'source "$1"; shift; run_with_deadline "$@"'
        if redirected:
            environment["FIXTURE_CALL_LOG"] = str(root / "call.log")
            invocation += ' >"$FIXTURE_CALL_LOG" 2>&1'
        # Source the helper explicitly; SSH startup files must not precede the
        # strict-mode/redirection oracle or write outside this fixture's log.
        result = subprocess.run(
            ["bash", "--noprofile", "--norc", *shell_flags, "-c", invocation,
             "deadline-fixture", str(HELPER), *(["--admit"] if admit else []), "0.25",
             *([] if admit else ["cargo", "fixture"]),
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

    def test_redirected_errexit_preserves_admission_and_execution_results(self):
        # C1 direct helper call has a shell redirection; C2 errexit is enabled,
        # with or without nounset; C3 admission either refuses or admits the
        # command; C4 the admitted command succeeds or returns 23/75/101.
        # Rules R1 refusal => exact 75 and no execution; R2 admitted => exact
        # command status/output and one execution. Every row must remove only
        # its temporary Cargo entry. Bash's redirected-errexit unwind otherwise
        # drops function locals before EXIT and replaces the real result with a
        # cleanup failure. Existing A1-A4 cover queue timing and actual timeout;
        # these rows isolate the missing shell-call boundary with the same owner.
        for shell_flags in (("-e",), ("-e", "-u")):
            for admission, execution in ((75, 0), (0, 0), (0, 23), (0, 75), (0, 101)):
                with self.subTest(flags=shell_flags, admission=admission, execution=execution), \
                     tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    marker = root / "executed"
                    result = self.cargo_fixture(
                        root,
                        ["bash", "-c", 'printf executed; : >"$1"; exit "$2"', "command"],
                        status=admission, delay=0, admit=True,
                        arguments=(str(marker), str(execution)),
                        shell_flags=shell_flags, redirected=True,
                    )
                    self.assertEqual(result.returncode, admission or execution, result.stderr)
                    self.assertEqual(result.stdout, "")
                    self.assertEqual(result.stderr, "", "cleanup must not escape the owned log")
                    self.assertEqual(marker.exists(), admission == 0)
                    self.assertEqual((root / "call.log").read_text(), "executed" if admission == 0 else "")

    def test_non_cargo_job_uses_the_existing_admission_and_deadline(self):
        # J1 an explicit whole job uses the same external Cargo entry: admission
        # lasts longer than its execution budget; non-Cargo argv/status remains
        # exact. J2 refusal emits 75 and starts no job. No new execution path.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            result = self.cargo_fixture(root, ['bash', '-c',
                'printf "%s" "$FIXTURE_ADMITTED"; exit 23'], admit=True)
            self.assertEqual((result.returncode, result.stdout), (23, '1'))
            marker = root / 'executed'
            result = self.cargo_fixture(root, ['touch', str(marker)], status=75, admit=True)
            self.assertEqual(result.returncode, 75)
            self.assertFalse(marker.exists())

    def test_cargo_execution_still_times_out_after_admission(self):
        # A4: C1 admission succeeds; C2 execution exceeds its unchanged budget.
        # Effects: admission reaches the execution entry, then returns 124.
        # D1-D4 below own real child readiness and the full TERM matrix; this
        # row isolates admission/dispatch without timing a second shell startup.
        with tempfile.TemporaryDirectory() as tmp:
            result = self.cargo_fixture(
                Path(tmp), ["bash", "-c", "exec sleep 100"], ready=True,
            )
            self.assertEqual(result.returncode, 124, result.stderr)
            self.assertEqual(result.stdout, "ready\n")

    def test_cargo_protocol_rejects_missing_or_mismatched_name(self):
        # A5: the external entry lacks Cargo's injected name or carries a
        # different one => reject with 2 before starting any requested command.
        # A1/A2 above own the matching-name/unchanged-argv positive rules.
        with tempfile.TemporaryDirectory() as tmp:
            entry = Path(tmp) / "cargo-fixture-deadline"
            marker = Path(tmp) / "executed"
            for arguments in ([], ["wrong-name", "1", "--", sys.executable, "-c",
                                  "from pathlib import Path; import sys; Path(sys.argv[1]).touch()",
                                  str(marker)]):
                with self.subTest(arguments=arguments):
                    # Parser unit: the successful external-command integration
                    # is A1/A2; invalid argv need no interpreter startup race.
                    self.assertEqual(deadline.main([str(entry), *arguments]), 2)
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
                        # Shorten only cleanup grace in this isolated Python
                        # invocation; production's five-second grace is unchanged.
                        driver = """
import importlib.util, os, selectors, subprocess, sys
from unittest.mock import ANY, patch
spec = importlib.util.spec_from_file_location('deadline', sys.argv[1])
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
m.KILL_GRACE_SECONDS = 0.15
process = subprocess.Popen(sys.argv[2:], start_new_session=True, stdout=subprocess.PIPE)
try:
    ready = selectors.DefaultSelector()
    ready.register(process.stdout, selectors.EVENT_READ)
    assert ready.select(10), 'D1-D4 fixture startup did not become ready'
    assert process.stdout.readline() == b'ready\\n'
    print('ready', flush=True)
    # Setup and execution are distinct causes: the actual process group is
    # ready before the tested execution clock. No cold-start SLO is claimed.
    with patch.object(m.subprocess, 'Popen', return_value=process) as spawn:
        result = m.run(0.5, sys.argv[2:])
    spawn.assert_called_once_with(sys.argv[2:], start_new_session=True, env=ANY)
    # The child's environment is the caller's plus this deadline's queue ledger.
    assert spawn.call_args.kwargs['env'][m.LEDGERS_VARIABLE].split(os.pathsep)[0].rsplit('/', 1)[-1].startswith(m.LEDGER_PREFIX)
    process.communicate(timeout=2)
    raise SystemExit(result)
finally:
    m.terminate_group(process)
"""
                        try:
                            result = subprocess.run(
                                [sys.executable, "-c", driver, str(DEADLINE),
                                 *self.group_command(coordinate, leader_ignores, child_ignores)],
                                capture_output=True, text=True, timeout=15,
                            )
                            self.assertEqual(result.returncode, 124, result.stderr)
                            self.assertEqual(result.stdout.strip(), "ready")
                        finally:
                            cleanup_fixture_group(coordinate)

    def test_driver_cancellation_reaps_the_same_ready_group(self):
        # C/E table S1 TERM or INT reaches the driver, after both actual group
        # members install their handlers. Cross all D1-D4 handler pairs: exact
        # 128+signal, closed inherited pipes, no live leader/child. Repeated TERM
        # during grace cannot restart cleanup or overwrite the first signal.
        # Wait for the driver to enter cleanup before sending the repeat.
        import selectors

        driver = """
import importlib.util, sys
spec=importlib.util.spec_from_file_location('deadline', sys.argv[1])
m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
m.KILL_GRACE_SECONDS=0.15
terminate_group=m.terminate_group

def observed_cleanup(process):
    print('cleaning', file=sys.stderr, flush=True)
    assert sys.stdin.readline().strip() == 'continue', 'observer did not release cleanup'
    terminate_group(process)

m.terminate_group=observed_cleanup
raise SystemExit(m.run(60,sys.argv[2:]))
"""
        with tempfile.TemporaryDirectory() as tmp:
            for number in (signal.SIGTERM, signal.SIGINT):
                for leader_ignores in (False, True):
                    for child_ignores in (False, True):
                        with self.subTest(signal=number, leader=leader_ignores, child=child_ignores):
                            coordinate = Path(tmp) / 'group'
                            ready = Path(str(coordinate) + '.ready')
                            coordinate.unlink(missing_ok=True)
                            ready.unlink(missing_ok=True)
                            process = subprocess.Popen(
                                [sys.executable, '-c', driver, str(DEADLINE),
                                 *self.group_command(coordinate, leader_ignores, child_ignores)],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
                            try:
                                until = time.monotonic() + 10
                                while not ready.exists() and time.monotonic() < until:
                                    time.sleep(0.01)
                                self.assertTrue(ready.exists(), 'S1 fixture startup')
                                members = json.loads(ready.read_text())
                                process.send_signal(number)
                                with selectors.DefaultSelector() as barrier:
                                    barrier.register(process.stderr, selectors.EVENT_READ)
                                    self.assertTrue(barrier.select(5), 'S1 driver entered cleanup')
                                    self.assertEqual(process.stderr.readline(), 'cleaning\n')
                                process.send_signal(signal.SIGTERM)
                                process.stdin.write('continue\n')
                                process.stdin.flush()
                                out, err = process.communicate(timeout=5)
                                self.assertEqual(process.returncode, 128 + number, err)
                                self.assertEqual(out.strip(), 'ready')
                                for pid in members.values():
                                    stat = Path(f'/proc/{pid}/stat')
                                    self.assertTrue(not stat.exists() or stat.read_text().split()[2] == 'Z',
                                                    'S1 no surviving live member')
                            finally:
                                cleanup_fixture_group(coordinate, process)

    def test_nonfinite_or_nonpositive_deadline_starts_nothing(self):
        # V1 zero/negative/NaN/infinity is not a finite execution deadline:
        # reject with 2 before any child effect. Valid deadlines retain D5.
        with tempfile.TemporaryDirectory() as tmp:
            marker = Path(tmp) / 'executed'
            for seconds in ('0', '-1', 'nan', 'inf'):
                result = subprocess.run([sys.executable, str(DEADLINE), seconds,
                                         '--', 'touch', str(marker)], capture_output=True, timeout=5)
                self.assertEqual(result.returncode, 2)
                self.assertFalse(marker.exists())

    def test_cancellation_during_spawn_retains_and_reaps_child(self):
        # S2 TERM arrives inside actual Popen construction, before run() retains
        # its returned handle => defer, retain the exact group, clean it, 143.
        # Prior handlers are restored after S3 ordinary completion. The bounded
        # isolated driver and final cleanup address only this fixture's group.
        driver = """
import importlib.util, os, signal, subprocess, sys
from pathlib import Path
from unittest.mock import patch
spec=importlib.util.spec_from_file_location('deadline',sys.argv[1])
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
m.KILL_GRACE_SECONDS=0.15
before={number:signal.getsignal(number) for number in (signal.SIGTERM,signal.SIGINT)}
real=subprocess.Popen
def spawn(*args,**kwargs):
    child=real(*args,**kwargs)
    Path(sys.argv[2]).write_text(str(child.pid))
    os.kill(os.getpid(),signal.SIGTERM)
    return child
with patch.object(m.subprocess,'Popen',side_effect=spawn):
    status=m.run(60,['sleep','60'])
assert all(signal.getsignal(number)==value for number,value in before.items())
assert m.run(2,['bash','-c','exit 0'])==0
assert all(signal.getsignal(number)==value for number,value in before.items())
raise SystemExit(status)
"""
        with tempfile.TemporaryDirectory() as tmp:
            coordinate = Path(tmp) / 'group'
            try:
                result = subprocess.run([sys.executable, '-c', driver, str(DEADLINE), str(coordinate)],
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 143, result.stderr)
                self.assertFalse(Path(f'/proc/{coordinate.read_text()}/stat').exists())
            finally:
                cleanup_fixture_group(coordinate)

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


@unittest.skipUnless(os.name == "nt", "Windows Job ownership requires native Windows")
class WindowsDeadlineTests(unittest.TestCase):
    @staticmethod
    def native_bash():
        return bash_executable()

    def test_bare_bash_selects_native_shell_before_windows_search(self):
        # W4 unqualified/case-varied Bash + poisoned current directory must
        # still use the configured native shell, retaining literal data and
        # both successful and refused command statuses inside the owned Job.
        with tempfile.TemporaryDirectory(prefix="native bash selection ") as tmp:
            root = Path(tmp)
            (root / "bash.exe").write_bytes(b"not an executable")
            literal = "literal /bin/sh;$(value) 中文"
            environment = dict(os.environ, AWAKEN_NATIVE_BASH=self.native_bash())
            for program in ("bash", "BASH.exe"):
                for status in (0, 31):
                    with self.subTest(program=program, status=status):
                        result = subprocess.run(
                            [sys.executable, str(DEADLINE), "30", "--", program, "-c",
                             'printf "%s" "$1"; exit "$2"', "native-selection", literal, str(status)],
                            cwd=root, env=environment, capture_output=True, timeout=40,
                        )
                        self.assertEqual(result.returncode, status, result.stderr)
                        self.assertEqual(result.stdout, literal.encode("utf-8"))

    def test_native_status_and_literal_arguments(self):
        # Docker's container paths and bind/tmpfs arguments must cross the
        # native Python boundary verbatim, just like shell-shaped data.
        arguments = ("with spaces", "'quotes'", "$(touch unexpected);`literal`",
                     "/bin/sh", "/data:rw,nosuid,nodev,noexec,size=4g",
                     "C:/fixture with spaces:/recovery", "/recovery/wal/%f")
        for status in (0, 23):
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                command = [Path(sys.executable).as_posix(), "-c",
                           f"import json, sys; print(json.dumps(sys.argv[1:])); raise SystemExit({status})",
                           *arguments]
                # Introduce literal argv at the shell-helper boundary. Git
                # Bash's native startup parser itself consumes bare apostrophes
                # in Windows argv; that earlier boundary is not the deadline.
                script = root / "literal-arguments.sh"
                script.write_text(f"source {shlex.quote(HELPER.as_posix())}\nrun_with_deadline 30 "
                                  + " ".join(shlex.quote(arg) for arg in command) + "\n",
                                  encoding="utf-8", newline="\n")
                entries = (
                    [sys.executable, str(DEADLINE), "30", "--", *command],
                    [self.native_bash(), script.as_posix()],
                )
                for entry in entries:
                    with self.subTest(status=status, entry=entry[0]):
                        result = subprocess.run(entry, cwd=root, capture_output=True, text=True,
                                                encoding="utf-8", errors="replace", timeout=40)
                        self.assertEqual(result.returncode, status, result.stderr)
                        self.assertEqual(json.loads(result.stdout), list(arguments))
                        self.assertFalse((root / "unexpected").exists())

    def test_native_cargo_dispatch_does_not_require_a_unix_addon(self):
        result = subprocess.run(
            [self.native_bash(), "-c", 'source "$1"; run_with_deadline 30 cargo --version',
             "native-cargo", HELPER.as_posix()], capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=40)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.startswith("cargo "), result.stdout)

    def test_native_shell_script_entry_retains_status_and_arguments(self):
        with tempfile.TemporaryDirectory(prefix="native shell entry ") as tmp:
            entry = Path(tmp) / "command with spaces.sh"
            entry.write_text('printf \'%s\\n\' "$1"\nexit 23\n', encoding="utf-8", newline="\n")
            result = subprocess.run(
                [self.native_bash(), "-c", 'source "$1"; run_with_deadline 30 "$2" "$3"',
                 "shell-entry", HELPER.as_posix(), entry.as_posix(), "/bin/sh"],
                capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=40)
            self.assertEqual(result.returncode, 23, result.stderr)
            self.assertEqual(result.stdout, "/bin/sh\n")

    def test_missing_job_refuses_command_before_effects(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = Path(tmp) / "executed"
            result = subprocess.run(
                [sys.executable, str(DEADLINE), "--windows-job-child", "awaken-ci-absent-job", "--",
                 sys.executable, "-c", "from pathlib import Path; import sys; Path(sys.argv[1]).touch()",
                 str(marker)], capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=15)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(marker.exists())

    def owned_tree(self, *, exit_leader=False, kill_owner=False):
        # W1 timeout with a live leader, W2 normal leader exit with a surviving
        # child, W3 deadline-owner death. All must close inherited stdout and
        # terminate the same real child, observed through a retained OS handle.
        # A TCP barrier lets this test retain the child handle before effects;
        # cleanup uses that handle, never a reused PID or unrelated process.
        import ctypes
        from ctypes import wintypes

        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.OpenProcess.argtypes = (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
        kernel.OpenProcess.restype = wintypes.HANDLE
        kernel.WaitForSingleObject.argtypes = (wintypes.HANDLE, wintypes.DWORD)
        kernel.WaitForSingleObject.restype = wintypes.DWORD
        kernel.TerminateProcess.argtypes = (wintypes.HANDLE, wintypes.UINT)
        kernel.CloseHandle.argtypes = (wintypes.HANDLE,)
        with tempfile.TemporaryDirectory(prefix="awaken-windows-deadline-") as tmp:
            root = Path(tmp)
            fixture = root / "owned-tree.py"
            fixture.write_text('''import os, socket, subprocess, sys, time
role = sys.argv[1]
if role != 'child':
    subprocess.Popen([sys.executable, __file__, 'child', sys.argv[2]])
host, port = sys.argv[2].rsplit(':', 1)
witness = socket.create_connection((host, int(port)), timeout=20)
witness.sendall(f"{role}:{os.getpid()}\\n".encode())
if role == 'child':
    # The test retains this connection while checking the process handle. No
    # fixture exit or PID reuse can precede the owner's termination effect.
    witness.settimeout(300)
    witness.recv(1)
else:
    assert witness.recv(1) == b'x', 'parent barrier was not released'
    print('ready', flush=True)
    if role == 'exit':
        raise SystemExit(23)
    time.sleep(300)
''', encoding="utf-8")
            listener = socket.socket()
            listener.bind(("127.0.0.1", 0))
            listener.listen(2)
            address = f"127.0.0.1:{listener.getsockname()[1]}"
            budget = "30" if exit_leader or kill_owner else "10"
            driver = subprocess.Popen(
                [sys.executable, str(DEADLINE), budget, "--", sys.executable, str(fixture),
                 "exit" if exit_leader else "wait", address],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
            child_handle = None
            participants = {}
            try:
                until = time.monotonic() + 8
                for _ in range(2):
                    listener.settimeout(max(0.001, until - time.monotonic()))
                    connection, _ = listener.accept()
                    connection.settimeout(max(0.001, until - time.monotonic()))
                    line = bytearray()
                    while not line.endswith(b"\n"):
                        chunk = connection.recv(1)
                        self.assertTrue(chunk, "owned participant exited before readiness")
                        line.extend(chunk)
                    role, pid = line.decode().strip().split(":")
                    self.assertNotIn(role, participants)
                    participants[role] = connection
                    if role == "child":
                        child_handle = kernel.OpenProcess(0x00100001, False, int(pid))
                self.assertTrue(child_handle, "cannot retain the owned child's synchronization/cleanup handle")
                participants["exit" if exit_leader else "wait"].sendall(b"x")
                if kill_owner:
                    driver.kill()
                output, errors = driver.communicate(timeout=15)
                if not kill_owner:
                    self.assertEqual(driver.returncode, 23 if exit_leader else 124, errors)
                    self.assertEqual(output.strip(), "ready")
                self.assertEqual(kernel.WaitForSingleObject(child_handle, 3000), 0,
                                 "owned child survived timeout, leader exit, or owner death")
            finally:
                if driver.poll() is None:
                    driver.kill()
                if child_handle:
                    if kernel.WaitForSingleObject(child_handle, 0) != 0:
                        kernel.TerminateProcess(child_handle, 1)
                    kernel.CloseHandle(child_handle)
                driver.communicate(timeout=5)
                for connection in participants.values():
                    connection.close()
                listener.close()

    def test_native_job_reaps_children_on_timeout_and_leader_exit(self):
        for exit_leader in (False, True):
            with self.subTest(exit_leader=exit_leader):
                self.owned_tree(exit_leader=exit_leader)

    def test_native_job_reaps_children_when_deadline_owner_dies(self):
        self.owned_tree(kill_owner=True)


@unittest.skipIf(os.name == "nt", "POSIX wait-lock cancellation")
class DeadlineWaitLockTests(unittest.TestCase):
    def test_signal_after_wait_lock_acquisition_reaps_the_child(self):
        # W1 acquire the real Popen waitpid lock, then deliver real TERM/INT
        # before acquire returns to Popen's try/finally. Asynchronous raises
        # leak that lock and deadlock cleanup. W2 a pending-only handler must
        # return the original signal status and reap the actual child within
        # the unchanged 1s execution / 10s observation / 5s cleanup budgets.
        driver = """
from pathlib import Path
import importlib.util, os, signal, subprocess, sys
from unittest.mock import patch
spec=importlib.util.spec_from_file_location('deadline_subject',sys.argv[1])
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
before={number:signal.getsignal(number) for number in (signal.SIGTERM,signal.SIGINT)}
real=subprocess.Popen
class SignalAtAcquire:
    def __init__(self, lock):
        self.lock=lock;self.fired=False
    def acquire(self, *args, **kwargs):
        acquired=self.lock.acquire(*args, **kwargs)
        if acquired and not self.fired:
            self.fired=True
            os.kill(os.getpid(),int(sys.argv[3]))
        return acquired
    def release(self):
        return self.lock.release()
    def __enter__(self):
        self.acquire();return self
    def __exit__(self, *args):
        self.release()
def spawn(*args, **kwargs):
    child=real(*args, **kwargs)
    child._waitpid_lock=SignalAtAcquire(child._waitpid_lock)
    coordinate=Path(sys.argv[2]);pending=coordinate.with_suffix('.pending')
    pending.write_text(str(child.pid));pending.replace(coordinate)
    return child
with patch.object(module.subprocess,'Popen',side_effect=spawn):
    status=module.run(1,[sys.executable,'-c','import time;time.sleep(120)'])
assert all(signal.getsignal(number)==handler for number,handler in before.items())
assert module.run(1,[sys.executable,'-c','pass'])==0
assert all(signal.getsignal(number)==handler for number,handler in before.items())
raise SystemExit(status)
"""
        for number in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(signal=number), tempfile.TemporaryDirectory() as tmp:
                coordinate = Path(tmp) / "group"
                process = subprocess.Popen(
                    [sys.executable, "-c", driver, str(DEADLINE), str(coordinate), str(int(number))],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                try:
                    output, errors = process.communicate(timeout=10)
                    self.assertEqual(process.returncode, 128 + number, (output, errors))
                    with self.assertRaises(ProcessLookupError):
                        os.kill(int(coordinate.read_text()), 0)
                finally:
                    cleanup_fixture_group(coordinate, process)


OLD_CAPABILITIES = {"schema_version": 1, "kind": "host-admission-capabilities",
                    "scheduler": "fair-single-lane-v1", "execution_slots": 1, "feedback_burst": 4,
                    "cooperative_formal": True, "full_gate_exclusive": True}
# Shape of the staged host tool f3e76a8909 (`cargo_admission.py --capabilities`).
NEW_CAPABILITIES = {"schema_version": 1, "kind": "host-admission-capabilities",
                    "scheduler": "fair-slots-v2", "scheduler_version": 2, "execution_slots": 2,
                    "exclusive_kinds": ["formal", "qualification", "profile=qualification"],
                    "feedback_burst": 4, "cooperative_formal": True,
                    "grant_environment": {"granted_at_unix": "AWAKEN_ADMISSION_GRANTED_AT",
                                          "queued_seconds": "AWAKEN_ADMISSION_QUEUED_SECONDS"},
                    "full_gate_exclusive": True,
                    "transient_pressure_wait": {"environment": "AWAKEN_ADMISSION_WAIT_SECONDS",
                                                "default_seconds": 1800}}


class CooperativeCapabilityTests(unittest.TestCase):
    def test_both_host_schedulers_are_accepted_and_others_refused(self):
        # K1 the old single-lane tool is accepted exactly as before; K2 the
        # multi-slot tool is accepted while formal stays exclusive; K3 a
        # multi-slot tool that would share a slot with formal is refused; K4 a
        # single-lane name with two slots or no cooperative root is refused.
        rows = (
            ("old single lane", OLD_CAPABILITIES, True),
            ("new slots, formal exclusive", NEW_CAPABILITIES, True),
            ("new slots, formal shared", {**NEW_CAPABILITIES, "exclusive_kinds": ["qualification"]}, False),
            ("new slots, no exclusive kinds", {k: v for k, v in NEW_CAPABILITIES.items()
                                               if k != "exclusive_kinds"}, False),
            ("single lane name, two slots", {**OLD_CAPABILITIES, "execution_slots": 2}, False),
            ("no cooperative root", {**NEW_CAPABILITIES, "cooperative_formal": False}, False),
            ("unknown scheduler", {**NEW_CAPABILITIES, "scheduler": "fair-slots-v3"}, False),
        )
        for label, value, accepted in rows:
            with self.subTest(label):
                self.assertEqual(deadline.cooperative_admission_problem(value) is None, accepted)

    def test_refusal_keeps_its_message_and_75(self):
        # The process entry _deadline.sh calls: accepted => 0 and silent;
        # refused => 75 with the existing "install the committed host policy" text.
        with tempfile.TemporaryDirectory() as tmp:
            observer = Path(tmp) / ("awaken-build-admission.cmd" if os.name == "nt"
                                    else "awaken-build-admission")
            for value, status in ((NEW_CAPABILITIES, 0), ({**NEW_CAPABILITIES, "exclusive_kinds": []}, 75)):
                with self.subTest(status=status):
                    observer.write_text(("@echo off\n@echo " + json.dumps(value) + "\n") if os.name == "nt"
                                        else "#!/bin/sh\nprintf '%s\\n' '" + json.dumps(value) + "'\n")
                    observer.chmod(0o755)
                    result = subprocess.run([sys.executable, str(DEADLINE), "--require-cooperative", str(observer)],
                                            capture_output=True, text=True, timeout=20)
                    self.assertEqual(result.returncode, status, result.stderr)
                    self.assertEqual("install the committed host policy" in result.stderr, status == 75)


@unittest.skipIf(os.name == "nt", "POSIX host admission")
class AdmissionQueueTests(unittest.TestCase):
    """A deadline wrapping an admitted Cargo call spends execution, not queue time.

    Fixture host: `cargo` waits FIXTURE_QUEUE seconds (the queue), then exports
    the grant when FIXTURE_GRANT=1, like cargo_admission.py f3e76a8909, and
    enters Cargo's external command. `awaken-build-admission --capabilities`
    reports the new or the old tool. An outer deadline shorter than the queue
    then measures the rule: Q1 a queued-then-run build is not killed for its
    queue; Q2 one that runs too long is still killed, saying the queue was not
    counted; Q3 with the old tool the outer clock counts the queue as before;
    Q4 a call queued past its bound stops with 75 and says why.
    """

    QUEUE = 5
    OUTER = 3

    def host(self, root, capabilities, grant):
        host = root / "host"
        host.mkdir()
        (host / "cargo").write_text("""#!/usr/bin/env bash
if [ "${AWAKEN_CARGO_ADMITTED:-}" != 1 ]; then
  sleep "$FIXTURE_QUEUE"
  export AWAKEN_CARGO_ADMITTED=1
  if [ "$FIXTURE_GRANT" = 1 ]; then
    export AWAKEN_ADMISSION_GRANTED_AT="$(date +%s.%N)" AWAKEN_ADMISSION_QUEUED_SECONDS="$FIXTURE_QUEUE"
  fi
fi
entry=$(command -v "cargo-$1") || exit 97
exec "$entry" "$@"
""")
        (host / "awaken-build-admission").write_text(
            "#!/bin/sh\n[ \"$1\" = --capabilities ] || exit 2\nprintf '%s\\n' '" + json.dumps(capabilities) + "'\n")
        for name in ("cargo", "awaken-build-admission"):
            (host / name).chmod(0o755)
        return dict({key: value for key, value in os.environ.items()
                     if not key.startswith(("AWAKEN_CARGO", "AWAKEN_ADMISSION", "AWAKEN_DEADLINE"))},
                    PATH=f"{host}:{os.environ['PATH']}", TMPDIR=str(root),
                    FIXTURE_QUEUE=str(self.QUEUE), FIXTURE_GRANT=str(int(grant)))

    def outer(self, environment, inner_budget, command):
        # outer deadline -> bash run_with_deadline --admit -> fixture Cargo queue -> inner deadline
        started = time.monotonic()
        result = subprocess.run(
            [sys.executable, str(DEADLINE), str(self.OUTER), "--", "bash", "--noprofile", "--norc", "-c",
             'source "$1"; shift; run_with_deadline --admit "$@"', "queue-fixture", str(HELPER),
             str(inner_budget), *command],
            env=environment, capture_output=True, text=True, timeout=60)
        return result, time.monotonic() - started

    def test_queued_then_run_build_is_not_killed_for_its_queue(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            environment = self.host(root, NEW_CAPABILITIES, grant=True)
            result, wall = self.outer(environment, 30, ["bash", "-c", "sleep 0.5; exit 23"])
            self.assertEqual(result.returncode, 23, result.stderr)
            self.assertGreater(wall, self.QUEUE, "the fixture queue must outlast the outer budget")
            self.assertEqual(sorted(path.name.split(".")[0] for path in root.iterdir()), ["host"],
                             "deadline entries and ledgers are retired")

    def test_build_that_runs_too_long_is_still_killed(self):
        with tempfile.TemporaryDirectory() as tmp:
            environment = self.host(Path(tmp), NEW_CAPABILITIES, grant=True)
            result, wall = self.outer(environment, 30, ["sleep", "100"])
            self.assertEqual(result.returncode, 124, result.stderr)
            self.assertGreater(wall, self.QUEUE + self.OUTER - 1)
            self.assertLess(wall, self.QUEUE + self.OUTER + 15)
            self.assertIn("queued for host build admission was not counted", result.stderr)

    def test_inner_budget_still_kills_after_admission(self):
        with tempfile.TemporaryDirectory() as tmp:
            environment = self.host(Path(tmp), NEW_CAPABILITIES, grant=True)
            started = time.monotonic()
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-c", 'source "$1"; shift; run_with_deadline --admit "$@"',
                 "queue-fixture", str(HELPER), "1", "sleep", "100"],
                env=environment, capture_output=True, text=True, timeout=60)
            self.assertEqual(result.returncode, 124, result.stderr)
            self.assertLess(time.monotonic() - started, self.QUEUE + 10)

    def test_old_host_tool_keeps_counting_the_queue(self):
        with tempfile.TemporaryDirectory() as tmp:
            environment = self.host(Path(tmp), OLD_CAPABILITIES, grant=False)
            result, wall = self.outer(environment, 30, ["bash", "-c", "exit 23"])
            self.assertEqual(result.returncode, 124, result.stderr)
            self.assertLess(wall, self.QUEUE, "the old clock stops the call while it is still queued")
            self.assertNotIn("not counted", result.stderr)

    def test_queue_past_its_bound_stops_with_75_and_names_the_queue(self):
        # In-process: the command never reaches a grant, so its own request stays
        # queued; past a 1s bound the deadline stops it instead of hanging.
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp, patch.object(deadline, "LEDGER_POLL_SECONDS", 0.2), \
                patch.object(tempfile, "tempdir", tmp):
            marker = Path(tmp) / "finished"
            started = time.monotonic()
            with patch("sys.stderr", new_callable=io.StringIO) as errors:
                status = deadline.run(0.5, ["bash", "-c", f'sleep 30; touch "{marker}"'], admission_bound=1.0)
            self.assertEqual(status, 75)
            self.assertLess(time.monotonic() - started, 10)
            self.assertFalse(marker.exists())
            self.assertIn("for a host build slot, past the 1s queue bound", errors.getvalue())
            self.assertEqual(list(Path(tmp).glob(deadline.LEDGER_PREFIX + "*")), [])

    def test_shell_budget_reads_its_ledger(self):
        # pg_tests.sh-style budget: open a ledger, run one queued admitted call,
        # read back at least the queue; close removes the ledger.
        with tempfile.TemporaryDirectory() as tmp:
            environment = self.host(Path(tmp), NEW_CAPABILITIES, grant=True)
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-c",
                 'source "$1"; awaken_budget_open || exit 9; run_with_deadline --admit 30 true || exit 8; '
                 'awaken_budget_queued_seconds; ledger=$awaken_budget_ledger; awaken_budget_close; '
                 '[ ! -e "$ledger" ] || exit 7', "budget-fixture", str(HELPER)],
                env=environment, capture_output=True, text=True, timeout=60)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertGreaterEqual(int(result.stdout.strip()), self.QUEUE - 1)



@unittest.skipIf(os.name == "nt", "POSIX host admission")
class AdmissionHolderTests(unittest.TestCase):
    def holder(self, root, status):
        observer = root / "awaken-build-admission"
        observer.write_text("#!/bin/sh\n[ \"$1\" = --status ] || exit 2\nprintf '%s\\n' '" + json.dumps(status) + "'\n")
        observer.chmod(0o755)
        state = root / "state"
        (state / "admission-queue").mkdir(parents=True, exist_ok=True)
        record = root / "queued.json"
        record.write_text(json.dumps({"status": "queued"}))
        (state / "admission-queue" / "ticket.json").write_text(json.dumps({"observation": str(record)}))
        result = subprocess.run(["bash", "--noprofile", "--norc", "-c", 'source "$1"; awaken_admission_holder',
                                 "holder-fixture", str(HELPER)],
                                env=dict(os.environ, AWAKEN_HOST_ADMISSION=str(observer), AWAKEN_ADMISSION_STATE=str(state)),
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def test_slot_table_is_preferred_and_tickets_remain_the_fallback(self):
        # H1 a host that reports scheduler.slots is described from that table;
        # H2 an older host (no slots in --status) falls back to queue tickets.
        with tempfile.TemporaryDirectory() as tmp:
            slots = {"scheduler": {"slots": [
                {"slot": 0, "state": "busy", "workload_kind": "feedback", "exclusive": False, "pid": 41,
                 "arguments": ["check", "-p", "demo"], "cwd": "/w/a", "held_seconds": 12.4},
                {"slot": 1, "state": "free"}], "queue": {"by_kind": {"feedback": 2, "formal": 1}}}}
            self.assertEqual(self.holder(Path(tmp), slots),
                             "1 of 2 slots busy; slot 0 feedback held 12s by pid 41 in /w/a: cargo check -p demo; "
                             "queued: feedback 2, formal 1")
            self.assertEqual(self.holder(Path(tmp), {"schema_version": 1, "eligible": True}),
                             "no current holder recorded; 1 queued")


if __name__ == "__main__":
    strict_unittest.main()
