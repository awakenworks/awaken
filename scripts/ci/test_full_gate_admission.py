#!/usr/bin/env python3
"""Process regression for the full gate's existing host admission boundary."""
import os
import shutil
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import ci_performance
from generate_hooks import local_imports
from _strict_unittest import main

# Actual execution subjects own the hook scope; the directory walk below
# inspects only this test's temporary fixture, never repository contents.
INPUT_GLOBS: tuple[str, ...] = (
    "scripts/ci/_deadline.sh",
    "scripts/ci/deadline.py",
    "scripts/ci/check-all.sh",
    "scripts/ci/_build_observation.sh",
    "scripts/ci/test_full_gate_admission.py",
)
ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / INPUT_GLOBS[0]


@unittest.skipIf(os.name == "nt", "the full-gate admission contract requires its Linux host")
class FullGateAdmissionTests(unittest.TestCase):
    def fixture(self, root):
        native = root / "rustup first"
        native.mkdir()
        (native / "cargo").write_text('#!/bin/sh\ntouch "$BAD_CARGO_MARKER"\nexit 99\n')
        (native / "cargo").chmod(0o755)
        host = root / "qualified host"
        host.mkdir()
        policy = """#!/usr/bin/env bash
case "${0##*/}" in
  awaken-build-admission) exit 98 ;;
esac
sleep "$FIXTURE_WAIT"
[ "$FIXTURE_STATUS" = 0 ] || exit "$FIXTURE_STATUS"
export AWAKEN_CARGO_ADMITTED=1
entry=$(command -v "cargo-$1") || exit 97
exec "$entry" "$@"
"""
        for name in ("cargo", "awaken-build-admission"):
            (host / name).write_text(policy)
            (host / name).chmod(0o755)
        # Git hooks and an outer gate carry live index/receipt coordinates.
        # The fixture owns neither; never let its nested gate consume them.
        env = dict({key: value for key, value in os.environ.items()
                    if not key.startswith(("GIT_", "CARGO_", "AWAKEN_"))},
                   PATH=f"{native}:{host}:{os.environ['PATH']}",
                   TMPDIR=str(root), BAD_CARGO_MARKER=str(root / "wrong-cargo-ran"),
                   FIXTURE_WAIT="0", FIXTURE_STATUS="0", INJECTION_MARKER=str(root / "unexpected"),
                   GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0")
        return host, env

    def invoke(self, root, env, *args):
        # The caller uses the same selection + scoped PATH as check-all. A native
        # Cargo first on PATH must never substitute for the admitted host policy.
        code = ('source "$1"; shift; original=$PATH; '
                'host_cargo_directory=$(awaken_host_cargo_directory) || exit "$?"; '
                'PATH="$host_cargo_directory:$PATH" run_with_deadline --admit 1 "$@"; '
                'status=$?; [ "$PATH" = "$original" ] || exit 96; exit "$status"')
        result = subprocess.run(["bash", "-c", code, "full-gate-fixture", str(HELPER), *args],
                                env=env, capture_output=True, text=True, timeout=12)
        self.assertFalse((root / "wrong-cargo-ran").exists())
        self.assertEqual(list(root.glob("awaken-cargo-deadline.*")), [])
        return result

    def test_fixture_detaches_parent_git_cargo_and_full_gate_coordinates(self):
        poison = {"GIT_DIR": "foreign.git", "GIT_INDEX_FILE": "foreign-index",
                  "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "core.hooksPath",
                  "GIT_CONFIG_VALUE_0": "foreign-hooks", "CARGO_TARGET_DIR": "foreign-target",
                  "AWAKEN_CARGO_ADMITTED": "1", "AWAKEN_HOST_ADMISSION": "foreign-host",
                  "AWAKEN_TEST_TIMINGS_FILE": "foreign-receipt",
                  "AWAKEN_FULL_GATE_LOG": "foreign-log", "AWAKEN_FULL_GATE_CANDIDATE": "f" * 40}
        with tempfile.TemporaryDirectory(prefix="full gate ") as tmp, patch.dict(os.environ, poison):
            _, env = self.fixture(Path(tmp))
            for key in poison:
                self.assertNotIn(key, env, key)
            self.assertEqual(env["GIT_CONFIG_GLOBAL"], os.devnull)
            self.assertEqual(env["GIT_CONFIG_NOSYSTEM"], "1")

    def test_native_cargo_first_still_waits_for_admission_and_preserves_outcome(self):
        for status in (0, 23):
            with self.subTest(status=status), tempfile.TemporaryDirectory(prefix="full gate ") as tmp:
                root = Path(tmp)
                host, env = self.fixture(root)
                env["FIXTURE_WAIT"] = "1.2"  # waiting exceeds the one-second execution budget
                arguments = ("with spaces", "'quotes'", '$(touch "$INJECTION_MARKER");`literal`', "")
                child = 'test "$AWAKEN_CARGO_ADMITTED" = 1 || exit 95; printf "%s\\0" "$@"; exit ' + str(status)
                result = self.invoke(root, env, "bash", "-c", child, "command", *arguments)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(result.stdout.split("\0")[:-1], list(arguments))
                self.assertFalse((root / "unexpected").exists())

    def test_explicit_observer_override_selects_its_matching_host_entry(self):
        with tempfile.TemporaryDirectory(prefix="full gate ") as tmp:
            root = Path(tmp)
            host, env = self.fixture(root)
            env["AWAKEN_HOST_ADMISSION"] = str(host / "awaken-build-admission")
            env["PATH"] = str(root / "rustup first") + os.pathsep + os.environ["PATH"]
            result = self.invoke(root, env, "bash", "-c", 'test "$AWAKEN_CARGO_ADMITTED" = 1')
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_or_different_host_cargo_refuses_before_any_command(self):
        for missing in (True, False):
            with self.subTest(missing=missing), tempfile.TemporaryDirectory(prefix="full gate ") as tmp:
                root = Path(tmp)
                host, env = self.fixture(root)
                if missing:
                    (host / "cargo").unlink()
                else:
                    (host / "cargo").write_text('#!/bin/sh\nexit 0\n')
                marker = root / "executed"
                result = self.invoke(root, env, "touch", str(marker))
                self.assertEqual(result.returncode, 75, result.stderr)
                self.assertIn("same host policy", result.stderr)
                self.assertFalse(marker.exists())

    def gate_checkout(self, root, env):
        checkout = root / "gate checkout"
        scripts = checkout / "scripts/ci"
        scripts.mkdir(parents=True)
        for relative in INPUT_GLOBS[:-1]:
            shutil.copy2(ROOT / relative, scripts / Path(relative).name)
        # The real entrypoint now owns a diagnostic context before queuing.
        # Copy its real Python import closure; the admission/process fakes
        # below still own only the host boundary exercised by this table.
        for module in [Path(ci_performance.__file__).name, *local_imports('ci_performance.py')]:
            shutil.copy2(ROOT / 'scripts/ci' / module, scripts / module)
        (scripts / "_provider_environment.sh").write_text(
            'awaken_unset_ambient_api_keys() { :; }\nawaken_append_repo_tools() { :; }\n')
        (scripts / "gate_tools.py").write_text('raise SystemExit(0)\n')
        (scripts / "_parallel_checks.sh").write_text(
            'parallel_checks_init() { [ "$AWAKEN_CARGO_ADMITTED" = 1 ] || exit 95; '
            'echo full-gate-admitted-once; exit 23; }\n')
        env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                   AWAKEN_BUILD_OBSERVATIONS=str(root / 'observations'),
                   AWAKEN_FULL_GATE_TIMEOUT_SECONDS="3")
        for command in (["git", "init", str(checkout)],
                        ["git", "-C", str(checkout), "-c", "user.name=fixture",
                         "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false",
                         "commit", "--allow-empty", "-m", "fixture"]):
            subprocess.run(command, env=env, capture_output=True, check=True)
        return checkout, scripts

    def test_real_full_gate_entry_admits_once_when_native_cargo_is_first(self):
        # Stop with a deliberate 23 at the first static queue, after the real
        # full-gate admission and log re-entry. No full-gate success is invented.
        with tempfile.TemporaryDirectory(prefix="full gate ") as tmp:
            root = Path(tmp)
            foreign_index = root / "foreign-index"
            outer_timing = root / "outer-timings.tsv"
            foreign_index.write_bytes(b"foreign-index-sentinel")
            outer_timing.write_bytes(b"outer-timing-sentinel")
            with patch.dict(os.environ, {"GIT_DIR": str(root / "foreign.git"),
                                       "GIT_INDEX_FILE": str(foreign_index),
                                       "AWAKEN_CARGO_ADMITTED": "1",
                                       "AWAKEN_FULL_GATE_LOG": str(root / "outer.log"),
                                       "AWAKEN_TEST_TIMINGS_FILE": str(outer_timing)}):
                host, env = self.fixture(root)
            checkout, scripts = self.gate_checkout(root, env)
            result = subprocess.run(["bash", str(scripts / "check-all.sh"), "--full-gate", "--record"],
                                    cwd=checkout, env=env, capture_output=True, text=True, timeout=12)
            self.assertEqual(result.returncode, 23, result.stderr + result.stdout)
            self.assertEqual(result.stdout.count("full-gate-admitted-once"), 1)
            self.assertFalse((root / "wrong-cargo-ran").exists())
            self.assertFalse((scripts / "gate_record.json").exists())
            self.assertEqual(foreign_index.read_bytes(), b"foreign-index-sentinel")
            self.assertEqual(outer_timing.read_bytes(), b"outer-timing-sentinel")

    def test_host_resource_refusal_retains_75_without_starting_child(self):
        with tempfile.TemporaryDirectory(prefix="full gate ") as tmp:
            root = Path(tmp)
            host, env = self.fixture(root)
            env["FIXTURE_STATUS"] = "75"
            marker = root / "executed"
            result = self.invoke(root, env, "touch", str(marker))
            self.assertEqual(result.returncode, 75, result.stderr)
            self.assertFalse(marker.exists())

    def test_formal_group_requires_admission_or_inherits_an_existing_exclusive_job(self):
        # G1 ordinary Formal + old/missing capability ->75 before gate body.
        # G2 an arbitrary legacy environment switch cannot bypass G1.
        # G3 an already admitted exclusive job -> body exactly once (fixture23),
        # without a cooperative re-entry or changing its lease contract.
        for inherited, switch, expected in ((False, '', 75), (False, 'exclusive', 75),
                                             (False, 'bogus', 75), (True, '', 23)):
            with self.subTest(inherited=inherited, switch=switch), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                _, env = self.fixture(root)
                checkout, scripts = self.gate_checkout(root, env)
                if inherited:
                    env['AWAKEN_CARGO_ADMITTED'] = '1'
                env['AWAKEN_FORMAL_SCHEDULING'] = switch
                result = subprocess.run(['bash', str(scripts / 'check-all.sh'), '--group', 'formal'],
                                        cwd=checkout, env=env, capture_output=True, text=True, timeout=12)
                self.assertEqual(result.returncode, expected, result.stderr + result.stdout)
                self.assertEqual(result.stdout.count('full-gate-admitted-once'), int(inherited))

    def test_cooperative_capability_contract_phase_markers_and_total_deadline(self):
        # C1 matching host capability -> root has phase mode, no exclusive
        # marker; child phase reacquires and sees the existing exclusive marker.
        # C2 initial admission wait exceeds budget -> excluded as before.
        # C3 later phase queue exceeds root budget -> root124, child never runs;
        # yielding cannot reset the total deadline. C4 old host ->75, no child.
        for phase_wait, expected in (('0', 23), ('1.2', 124)):
            with self.subTest(phase_wait=phase_wait), tempfile.TemporaryDirectory(prefix='formal phases ') as tmp:
                root = Path(tmp)
                host, env = self.fixture(root)
                policy = '''#!/usr/bin/env bash
if [ "${0##*/}" = awaken-build-admission ]; then
  echo '{"schema_version":1,"kind":"host-admission-capabilities","scheduler":"fair-single-lane-v1","execution_slots":1,"cooperative_formal":true}'
  exit 0
fi
if [ "${AWAKEN_ADMISSION_MODE:-}" = cooperative ]; then
  sleep 1.2
  unset AWAKEN_ADMISSION_MODE AWAKEN_CARGO_ADMITTED
  export AWAKEN_ADMISSION_PHASE_MODE=1
else
  sleep "$PHASE_WAIT"
  export AWAKEN_CARGO_ADMITTED=1
fi
entry=$(command -v "cargo-$1") || exit 97
exec "$entry" "$@"
'''
                for name in ('cargo', 'awaken-build-admission'):
                    (host / name).write_text(policy)
                env['PHASE_WAIT'] = phase_wait
                marker = root / 'phase-executed'
                phase = 'test "$AWAKEN_CARGO_ADMITTED" = 1 || exit 95; touch "$1"; exit 23'
                body = ('test "${AWAKEN_CARGO_ADMITTED:-}" != 1 || exit 94; '
                        'test "$AWAKEN_ADMISSION_PHASE_MODE" = 1 || exit 93; '
                        'source "$1"; run_admitted_phase 2 bash -c "$2" phase "$3"')
                result = subprocess.run(['bash', '-c', 'source "$1"; shift; run_with_deadline --cooperative-admit 1 "$@"',
                                         'fixture', str(HELPER), 'bash', '-c', body, 'root', str(HELPER), phase, str(marker)],
                                        env=env, capture_output=True, text=True, timeout=12)
                self.assertEqual(result.returncode, expected, result.stderr + result.stdout)
                self.assertEqual(marker.exists(), expected == 23)
                self.assertFalse((root / 'wrong-cargo-ran').exists())
        with tempfile.TemporaryDirectory(prefix='old formal host ') as tmp:
            root = Path(tmp)
            _, env = self.fixture(root)
            marker = root / 'executed'
            result = subprocess.run(['bash', '-c', 'source "$1"; run_with_deadline --cooperative-admit 1 touch "$2"',
                                     'fixture', str(HELPER), str(marker)], env=env, capture_output=True, text=True, timeout=12)
            self.assertEqual(result.returncode, 75, result.stderr)
            self.assertFalse(marker.exists())


if __name__ == "__main__":
    main()
