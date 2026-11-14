#!/usr/bin/env python3
"""Process regression for the full gate's existing host admission boundary."""
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import ci_performance
from generate_hooks import local_imports
from _strict_unittest import main
from _host_admission_fixture import host_admission_fixture

# Actual execution subjects own the hook scope; the directory walk below
# inspects only this test's temporary fixture, never repository contents.
INPUT_GLOBS: tuple[str, ...] = (
    "scripts/ci/_deadline.sh",
    "scripts/ci/deadline.py",
    "scripts/ci/check-all.sh",
    "scripts/ci/_build_observation.sh",
    "scripts/ci/_cargo_target.sh",
    "scripts/ci/test_full_gate_admission.py",
)
ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / INPUT_GLOBS[0]


@unittest.skipIf(os.name == "nt", "the full-gate admission contract requires its Linux host")
class FullGateAdmissionTests(unittest.TestCase):
    def fixture(self, root):
        return host_admission_fixture(root)

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

    def test_generic_admit_selects_the_same_host_when_rustup_shadows_cargo(self):
        # S1 observer remains discoverable after native Cargo on PATH; S2 the
        # ordinary Python/Shell --admit entries run without a full-gate caller
        # selecting Cargo first. Both must execute the matching host policy,
        # export its actual admission and preserve status/argv; native Cargo
        # must never run. Existing full-gate rows own required-host refusal.
        for entry in ("python", "shell"):
            with self.subTest(entry=entry), tempfile.TemporaryDirectory(prefix="admit shadow ") as tmp:
                root = Path(tmp)
                _, env = self.fixture(root)
                child = ["bash", "-c", 'test "$AWAKEN_CARGO_ADMITTED" = 1 || exit 95; printf admitted']
                command = ([sys.executable, str(ROOT / "scripts/ci/deadline.py"), "--admit", "1", "--", *child]
                           if entry == "python" else
                           ["bash", "--noprofile", "--norc", "-c",
                            'source "$1"; shift; run_with_deadline --admit 1 "$@"',
                            "generic-admit", str(HELPER), *child])
                result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=12)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "admitted")
                self.assertFalse((root / "wrong-cargo-ran").exists(), "S1 native Cargo cannot replace admission")

    def test_generic_admit_refuses_a_broken_or_missing_explicit_host_before_execution(self):
        # S3 observer found but its Cargo is absent/different, or an explicit
        # observer names a missing file => 75, no native-Cargo fallback and no
        # command. Unlike N1's absent optional host, a configured broken host
        # cannot silently become an ordinary unadmitted platform.
        for damage in ("missing cargo", "different cargo", "missing override"):
            with self.subTest(damage=damage), tempfile.TemporaryDirectory(prefix="broken admit ") as tmp:
                root = Path(tmp)
                host, env = self.fixture(root)
                if damage == "missing cargo":
                    (host / "cargo").unlink()
                elif damage == "different cargo":
                    (host / "cargo").write_text("#!/bin/sh\nexit 0\n")
                else:
                    env["AWAKEN_HOST_ADMISSION"] = str(root / "missing observer")
                marker = root / "executed"
                result = subprocess.run(
                    [sys.executable, str(ROOT / "scripts/ci/deadline.py"), "--admit", "1", "--", "touch", str(marker)],
                    env=env, capture_output=True, text=True, timeout=12,
                )
                self.assertEqual(result.returncode, 75, result.stderr)
                self.assertFalse(marker.exists())
                self.assertFalse((root / "wrong-cargo-ran").exists())

    def test_explicit_observer_override_selects_its_matching_host_entry(self):
        # S4 absolute/relative observer overrides select the same normalized
        # pair even when the observer directory is absent from the caller PATH.
        for relative in (False, True):
            with self.subTest(relative=relative), tempfile.TemporaryDirectory(prefix="full gate ") as tmp:
                root = Path(tmp)
                host, env = self.fixture(root)
                observer = host / "awaken-build-admission"
                env["AWAKEN_HOST_ADMISSION"] = os.path.relpath(observer) if relative else str(observer)
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
        # The fixture's copy of check-all.sh runs, not the repository's: what it
        # executes is the files copied above (named as this test's subjects)
        # and the fakes written here, never check-all.sh's own callees.
        return checkout, scripts, scripts / "check-all.sh"

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
            checkout, scripts, gate = self.gate_checkout(root, env)
            result = subprocess.run(["bash", str(gate), "--full-gate", "--record"],
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
                checkout, scripts, gate = self.gate_checkout(root, env)
                if inherited:
                    env['AWAKEN_CARGO_ADMITTED'] = '1'
                env['AWAKEN_FORMAL_SCHEDULING'] = switch
                result = subprocess.run(['bash', str(gate), '--group', 'formal'],
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
