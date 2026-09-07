"""Execution-receipt decision tables; no fixture claims to prove Rust code."""

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest

from formal_run_evidence import ROOT, begin_record, finish_record, validate_record, write_record, begin_artifacts, finish_artifacts, verify_artifacts


def prepare_source_fixture(root: Path, extra_sources: dict[str, str] | None = None) -> None:
    """Shared synthetic Git/receipt fixture; it never attests a real proof run."""
    root.mkdir()
    for relative in ("scripts/ci/formal_run_evidence.py", "scripts/ci/check_formal.sh",
                     "scripts/ci/bootstrap_kani.sh", "scripts/ci/_cargo_target.sh", "scripts/ci/_provider_environment.sh", "scripts/ci/check-reliability.sh", "rust-toolchain.toml"):
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / relative, target)
    for relative, source in {".gitignore": "target/\n", "transitive.rs": "// dependency\n",
                             **(extra_sources or {})}.items():
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(source)
    for arguments in (("init", "--quiet"), ("config", "core.autocrlf", "false"), ("add", "."),
                      ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                       "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "fixture")):
        subprocess.run(["git", *arguments], cwd=root, check=True, capture_output=True)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="awaken-proof-receipt-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "repo"
        prepare_source_fixture(self.root)

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, stderr=subprocess.STDOUT)

    def test_kani_build_dir_matches_the_driver_target_directory(self):
        # Cause/effect graph: C1 an explicit Cargo target directory and C2 a
        # user-level build.build-dir are both active. E1 run_kani must override
        # only the Kani subprocess build directory to <target>/kani, because
        # that is the location the existing Kani driver reads after compiling;
        # E2 the package, job count, and harness arguments remain unchanged.
        # Decision table: R1 C1+C2 -> E1+E2. Omitting C2 is not a separate rule:
        # the same exact path is valid and keeps one invocation owner.
        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        body = source.split("  run_kani() {\n", 1)[1].split("\n  }", 1)[0]
        function = "run_kani() {\n" + body + "\n}"
        bindir = Path(self.temporary.name) / "kani-bin"
        bindir.mkdir()
        cargo = bindir / "cargo"
        cargo.write_text(
            '#!/bin/sh\nprintf \'%s\\n%s\\n\' "$CARGO_BUILD_BUILD_DIR" "$*" > "$CALLS"\n'
        )
        cargo.chmod(0o755)
        calls = bindir / "calls"
        target = Path(self.temporary.name) / "cargo-target"
        script = (
            "set -euo pipefail\n"
            f"CARGO_TARGET_DIR={shlex.quote(str(target))}\n"
            "kani_jobs=1\n"
            + function
            + "\nrun_kani awaken-dispatch --no-default-features --harness exact_claim\n"
        )
        result = subprocess.run(
            ["/bin/bash", "-c", script],
            env=dict(os.environ, PATH=str(bindir), CALLS=str(calls)),
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = calls.read_text().splitlines()
        self.assertEqual(lines[0], str(target / "kani"))
        self.assertEqual(
            lines[1],
            "kani -p awaken-dispatch --output-format terse -j 1 "
            "--no-default-features --harness exact_claim",
        )

    def test_pinned_solver_selection_and_strict_missing_tool_policy(self):
        # Z1 installed exact version + proof success => invoke the exact harness.
        # Z2 wrong/unreadable version => fail before proof. Z3 missing solver =>
        # mark missing, which fails strict mode and is explicit in diagnostic mode.
        # Z4 proof failure => propagate. Execute the real runner selection and its
        # final strict check with a solver process stub; this is not proof evidence.
        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        selection = "  if command -v z3" + source.split("  if command -v z3", 1)[1].split("  run_kani awaken-protocol-managed \\\n", 1)[0]
        strict_check = "\nif [ \"$require_tools\"" + source.rsplit("\nif [ \"$require_tools\"", 1)[1]
        version = begin_record(self.root, True, 100)["proof_toolchain"]["z3"]
        for installed, reported, version_exit, proof_exit, strict, expected in (
            (True, version, 0, 0, 1, 0),
            (True, "wrong", 0, 0, 1, 1),
            (True, version, 31, 0, 1, 31),
            (False, "", 0, 0, 1, 1),
            (False, "", 0, 0, 0, 0),
            (True, version, 0, 23, 1, 23),
        ):
            with self.subTest(installed=installed, reported=reported, strict=strict, proof_exit=proof_exit), tempfile.TemporaryDirectory() as temporary:
                bindir = Path(temporary)
                if installed:
                    tool = bindir / "z3"
                    tool.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(reported) + f"\nexit {version_exit}\n")
                    tool.chmod(0o755)
                calls = bindir / "calls"
                script = f"set -euo pipefail\nmissing=0\nrequire_tools={strict}\nz3_version={shlex.quote(version)}\n"
                script += "run_kani() { printf '%s\\n' \"$*\" > \"$CALLS\"; return \"$PROOF_EXIT\"; }\n"
                result = subprocess.run(["/bin/bash", "-c", script + selection + strict_check],
                                        env=dict(os.environ, PATH=str(bindir), CALLS=str(calls), PROOF_EXIT=str(proof_exit)),
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, expected, result.stderr)
                invoked = installed and reported == version and version_exit == 0
                self.assertEqual(calls.exists(), invoked)
                if invoked:
                    self.assertEqual(calls.read_text().strip(), "awaken-protocol-managed --solver z3 --harness memory_time_filter_compares_visible_milliseconds_without_merging_them")
                if not installed:
                    self.assertIn("skipped Z3", result.stdout)
        passed = finish_record(begin_record(self.root, True, 100), self.root, False, 110)
        changed = dict(passed, proof_toolchain=dict(passed["proof_toolchain"], z3="wrong"))
        with self.assertRaisesRegex(ValueError, "toolchain"):
            validate_record(changed, self.root, 110)

    def test_artifact_success_failure_and_source_binding(self):
        # A1 unchanged source + exit 0 -> Passed; A2 exit 23 -> Failed with
        # identical raw/model/log bytes retained; A3 changed source -> Failed.
        # Artifacts are diagnostic receipts, never independent release proofs.
        for code in (0, 23):
            directory = begin_artifacts(self.root, self.root / "target/evidence", "session-observations")
            inputs = {"raw/trace.json": b'{"sequence":0}\n', "rendered/Trace.tla": b'fixture\n', "suite.log": b'failure or success\n'}
            for relative, contents in inputs.items():
                path = directory / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(contents)
            record = finish_artifacts(directory, self.root, code)
            self.assertEqual(record["status"], "passed" if code == 0 else "failed")
            self.assertEqual(record["exit_code"], code)
            self.assertEqual(verify_artifacts(directory, self.root), record)
            for field, changed in [("status", "failed" if code == 0 else "passed"), ("exit_code", 23 if code == 0 else 0), ("proof_toolchain", {})]:
                write_record(directory / "evidence.json", dict(record, **{field: changed}))
                with self.assertRaises(ValueError): verify_artifacts(directory, self.root)
                write_record(directory / "evidence.json", record)
            for relative, contents in inputs.items(): self.assertEqual((directory / relative).read_bytes(), contents)
            for relative in inputs:
                path = directory / relative
                contents = path.read_bytes()
                path.write_bytes(contents + b'changed')
                with self.assertRaisesRegex(ValueError, "content mismatch"): verify_artifacts(directory, self.root)
                path.unlink()
                with self.assertRaisesRegex(ValueError, "content mismatch"): verify_artifacts(directory, self.root)
                path.write_bytes(contents)
        directory = begin_artifacts(self.root, self.root / "target/evidence", "formal")
        (self.root / "transitive.rs").write_text("changed")
        self.assertEqual(finish_artifacts(directory, self.root, 0)["status"], "failed")
        with self.assertRaisesRegex(ValueError, "source mismatch"): verify_artifacts(directory, self.root)

    def test_artifact_invocations_cannot_reuse_success(self):
        # A4 rerun creates a new identity/directory; A5 explicit existing output
        # fails instead of mixing runs; A6 Running/incomplete never validates.
        parent = self.root / "target/evidence"
        first = begin_artifacts(self.root, parent, "formal")
        finish_artifacts(first, self.root, 0)
        second = begin_artifacts(self.root, parent, "formal")
        self.assertNotEqual(first, second)
        with self.assertRaisesRegex(ValueError, "incomplete"): verify_artifacts(second, self.root)
        with self.assertRaises(FileExistsError): begin_artifacts(self.root, parent, "formal", first)
        self.assertEqual(verify_artifacts(first, self.root)["status"], "passed")

    def test_completion_requires_strict_unchanged_inputs_and_all_tools(self):
        # R1 strict+unchanged+all tools succeeds; R2 optional/R3 missing tools/
        # R4 source changes/R5 time reverses/R6 second completion all reject.
        record = begin_record(self.root, True, 100)
        passed = finish_record(record, self.root, False, 110)
        validate_record(passed, self.root, 110)
        for changed, missing, now in (
            (dict(record, strict=False), False, 110), (record, True, 110),
            (record, False, 99), (passed, False, 111),
        ):
            with self.subTest(missing=missing, now=now, status=changed["status"]):
                with self.assertRaises(ValueError):
                    finish_record(changed, self.root, missing, now)
        (self.root / "transitive.rs").write_text("// changed dependency\n")
        with self.assertRaisesRegex(ValueError, "source changed"):
            finish_record(record, self.root, False, 110)

    def test_every_release_identity_axis_is_binding(self):
        # V2-V4: take a valid completed receipt, change one axis at a time.
        # Every mutation rejects independently; the positive row prevents a
        # reject-everything implementation from satisfying this decision table.
        passed = finish_record(begin_record(self.root, True, 100), self.root, False, 110)
        validate_record(passed, self.root, 110)
        mutations = (
            ("status", "running"), ("strict", False), ("schema_version", 0),
            ("run_id", ""), ("started_at_unix", 120), ("finished_at_unix", 111),
            ("source", dict(passed["source"], commit="a" * 40)),
            ("source", dict(passed["source"], tree="a" * 40)),
            ("source", dict(passed["source"], clean=False)),
            ("proof_toolchain", {}), ("runner", dict(passed["runner"], digest="sha256:" + "0" * 64)),
        )
        for key, value in mutations:
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                validate_record(dict(passed, **{key: value}), self.root, 110)

    def test_new_invocation_retires_old_success_and_atomic_file_is_valid(self):
        # C1 previous Passed; C2 new Running. E1 the same output path no longer
        # validates until the new invocation completes. No second report store.
        path = self.root / "target/formal-run.json"
        first = begin_record(self.root, True, 100)
        write_record(path, finish_record(first, self.root, False, 110))
        validate_record(json.loads(path.read_text()), self.root, 110)
        second = begin_record(self.root, True, 120)
        self.assertNotEqual(first["run_id"], second["run_id"])
        write_record(path, second)
        with self.assertRaises(ValueError):
            validate_record(json.loads(path.read_text()), self.root, 120)
        self.assertEqual([p.name for p in path.parent.iterdir()], [path.name])

    def test_actual_gate_failure_leaves_running_not_stale_success(self):
        # C1 a completed previous run; C2 the actual check_formal.sh starts;
        # C3 its first checker fails with 23. R=C1+C2+C3 => nonzero body and
        # Running receipt, with no Cargo/proof call and no stale green reuse.
        path = self.root / "target/test-timings/formal-run.json"
        write_record(path, finish_record(begin_record(self.root, True, 100), self.root, False, 110))
        bindir = Path(self.temporary.name) / "bin"
        bindir.mkdir()
        shim = bindir / "python3"
        shim.write_text("#!/bin/sh\ncase \"$1\" in\n"
                        "  scripts/ci/formal_run_evidence.py) exec " + shlex.quote(sys.executable) + " \"$@\" ;;\n"
                        "  *) exit 23 ;;\nesac\n")
        shim.chmod(0o755)
        env = dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                   AWAKEN_FORMAL_RUN_EVIDENCE=str(path))
        result = subprocess.run(["bash", "scripts/ci/check_formal.sh", "--require-tools"],
                                cwd=self.root, env=env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertEqual(json.loads(path.read_text())["status"], "running")
        bundles = list((self.root / "target/test-timings").glob("formal-*/evidence.json"))
        self.assertEqual(len(bundles), 1)
        failure = verify_artifacts(bundles[0].parent, self.root)
        self.assertEqual((failure["status"], failure["exit_code"]), ("failed", 23))

    def test_actual_completion_trap_seals_before_publishing_success(self):
        # A8 body succeeds + sealing succeeds -> Passed; A9 body succeeds +
        # sealing fails -> nonzero and Running. Execute the real script's
        # initialization/EXIT trap, replacing only the expensive proof body.
        # This tests publication ordering, not a successful formal proof run.
        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        initialization, _ = source.split("\npython3 scripts/ci/check_feature_coverage.py", 1)
        bindir = Path(self.temporary.name) / "completion-bin"
        bindir.mkdir()
        shim = bindir / "python3"
        shim.write_text('#!/bin/sh\nif [ "$2" = artifacts-finish ] && [ "$FAIL_SEAL" = 1 ]; then exit 37; fi\nexec '
                        + shlex.quote(sys.executable) + ' "$@"\n')
        shim.chmod(0o755)
        path = self.root / "target/test-timings/formal-run.json"
        for fail_seal in (False, True):
            with self.subTest(fail_seal=fail_seal):
                env = dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                           FAIL_SEAL=str(int(fail_seal)), AWAKEN_FORMAL_RUN_EVIDENCE=str(path))
                result = subprocess.run(["bash", "-c", initialization + "\nexit 0\n",
                                         "scripts/ci/check_formal.sh", "--require-tools"],
                                        cwd=self.root, env=env, capture_output=True, text=True, timeout=30)
                self.assertEqual(result.returncode, 1 if fail_seal else 0, result.stderr)
                self.assertEqual(json.loads(path.read_text())["status"], "running" if fail_seal else "passed")

    def test_actual_native_failure_retains_partial_trace_and_log(self):
        # A7 the actual native composition runs, first Cargo writes partial
        # diagnostic evidence then fails with 31. The pipeline/EXIT trap must
        # retain bytes and exact status, and must not continue to PNPM. The shim
        # tests runner wiring only; it is not a passing Rust or native OS result.
        bindir = Path(self.temporary.name) / "native-bin"
        bindir.mkdir()
        cargo = bindir / "cargo"
        cargo.write_text('#!/bin/sh\nmkdir -p "$AWAKEN_OBSERVATION_TRACE_DIR"\nprintf \'{"partial":true}\\n\' > "$AWAKEN_OBSERVATION_TRACE_DIR/partial.json"\necho native-failure-marker\nexit 31\n')
        cargo.chmod(0o755)
        pnpm = bindir / "pnpm"
        pnpm.write_text('#!/bin/sh\ntouch "' + str(self.root / "unexpected-pnpm") + '"\nexit 0\n')
        pnpm.chmod(0o755)
        env = dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"])
        env.pop("AWAKEN_SESSION_EVIDENCE_DIR", None)
        result = subprocess.run(["bash", "scripts/ci/check-reliability.sh", "--session-observations"],
            cwd=self.root, env=env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 31, result.stderr)
        self.assertFalse((self.root / "unexpected-pnpm").exists())
        bundles = list((self.root / "target/test-timings").glob("session-observations-*/evidence.json"))
        self.assertEqual(len(bundles), 1)
        directory = bundles[0].parent
        record = verify_artifacts(directory, self.root)
        self.assertEqual((record["status"], record["exit_code"]), ("failed", 31))
        self.assertEqual((directory / "raw/partial.json").read_text(), '{"partial":true}\n')
        self.assertIn("native-failure-marker", (directory / "suite.log").read_text())

    def test_dirty_diagnostic_run_never_qualifies_release(self):
        # A stable dirty tree may complete for local diagnosis; it cannot
        # attest a release commit, including newly added/untracked Rust input.
        (self.root / "untracked.rs").write_text("// new module\n")
        record = finish_record(begin_record(self.root, True, 100), self.root, False, 110)
        with self.assertRaisesRegex(ValueError, "clean release source"):
            validate_record(record, self.root, 110)

    def test_git_normalized_windows_checkout_preserves_source_identity(self):
        # E1 a clean LF checkout completes; E2 Windows-style autocrlf checkout
        # changes physical bytes but not Git source. The same receipt validates.
        # E3 a semantic edit after conversion still rejects, so normalization
        # cannot conceal a changed runner or weaken the clean-source boundary.
        runner = self.root / "scripts/ci/check_formal.sh"
        self.git("config", "core.autocrlf", "false")
        self.assertFalse(b"\r\n" in runner.read_bytes(), "LF fixture")
        passed = finish_record(begin_record(self.root, True, 100), self.root, False, 110)
        windows = self.root.parent / "windows-checkout"
        subprocess.run(["git", "clone", "--quiet", "--no-hardlinks", "--config", "core.autocrlf=true",
                        str(self.root), str(windows)], check=True, capture_output=True)
        runner = windows / "scripts/ci/check_formal.sh"
        self.assertTrue(b"\r\n" in runner.read_bytes(), "CRLF fixture")
        validate_record(passed, windows, 110)
        runner.write_bytes(runner.read_bytes() + b"exit 23\r\n")
        with self.assertRaisesRegex(ValueError, "clean release source"):
            validate_record(passed, windows, 110)


if __name__ == "__main__":
    unittest.main()
