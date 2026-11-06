"""Execution-receipt decision tables; no fixture claims to prove Rust code."""

import json
import os
from pathlib import Path
import select
import signal
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
import _strict_unittest as strict_unittest
from _native_shell import bash_executable

import formal_budget
import formal_plan
import formal_sources
from formal_sources import detached_git_environment
from formal_run_evidence import FULL_SELECTION, ROOT, begin_record, finish_record, validate_record, write_record, begin_artifacts, finish_artifacts, verify_artifacts, source_snapshot, digest_bytes, committed_file_digest, proof_toolchain



def run_shell_fixture(arguments, **kwargs):
    """Keep multiline shell payloads out of native Windows argv quoting.

    The file contains the exact gate slice. A short source trampoline retains
    its original $0 and positional arguments, so receipt/EXIT behavior remains
    the behavior under test. The file stays outside the fixture's Git source.
    """
    if arguments[1:2] != ["-c"]:
        return subprocess.run(arguments, **kwargs)
    script, *positionals = arguments[2:]
    argv0 = positionals[0] if positionals else arguments[0]
    with tempfile.TemporaryDirectory(prefix="awaken-formal-shell-") as directory:
        entry = Path(directory) / "gate-slice.sh"
        entry.write_text(script, encoding="utf-8", newline="\n")
        return subprocess.run([arguments[0], "-c", 'entry=$1; shift; source "$entry"',
                               argv0, entry.as_posix(), *positionals[1:]], **kwargs)


def nested_shell_function(source: str, name: str) -> str:
    marker = f"  {name}() {{\n"
    body = source.split(marker, 1)[1].split("\n  }", 1)[0]
    return f"{name}() {{\n{body}\n}}"


def formal_gate_initialization(root: Path) -> str:
    source = (root / "scripts/ci/check_formal.sh").read_text()
    # This slice owns receipt/trap behavior, while gate_tools owns real tool
    # and disk admission. Keep both exact calls but supply that independent
    # port explicitly; a host's Kani installation must not decide these cases.
    admission = '''python3() {
  if [ "$1" = scripts/ci/gate_tools.py ]; then
    [ "$#" -eq 4 ] && [ "$4" = formal ] || return 2
    case "$2:$3" in require:--gate|disk:--stage) return 0 ;; *) return 2 ;; esac
  fi
  command python3 "$@"
}
'''
    return admission + source.split("\npython3 scripts/ci/check_feature_coverage.py", 1)[0]


# Entry points the fixture executes, plus the data files they read that no
# script names in its own text.
FIXTURE_ROOTS = (
    "scripts/ci/formal_run_evidence.py",
    "scripts/ci/check_formal.sh",
    "scripts/ci/check-reliability.sh",
    "scripts/ci/bootstrap_kani.sh",
    "scripts/ci/patches/kani-wide-niche.patch",
    "scripts/ci/patches/cbmc-smt-type-dependencies.patch",
    "scripts/ci/fixtures/cbmc_recursive_alias.c",
    "scripts/ci/fixtures/kani_wide_niche.rs",
    "rust-toolchain.toml",
)


def fixture_sources() -> list[str]:
    """Every repository file the fixture's entry scripts can reach.

    The closure is the formal plan's own (``formal_plan.script_closure``):
    each script's `scripts/ci/...` references and sibling imports, followed
    transitively. A script that grows a dependency therefore brings it into
    the fixture by itself; nothing has to be remembered here.
    """

    return sorted(formal_plan.script_closure(ROOT, FIXTURE_ROOTS))


def prepare_source_fixture(root: Path, extra_sources: dict[str, str] | None = None,
                           *, object_format: str | None = None) -> None:
    """Shared synthetic Git/receipt fixture; it never attests a real proof run."""
    root.mkdir()
    # The fixture copies the entry scripts' *dependency closure*, not a
    # hand-kept list. A hand-kept list is a guardrail narrower than its
    # subject: adding gate_tools.py to check_formal.sh left this list behind
    # and the gate failed inside its own self-test with "can't open file".
    for relative in fixture_sources():
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / relative, target)
    for relative, source in {".gitignore": "target/\n", "transitive.rs": "// dependency\n",
                             **(extra_sources or {})}.items():
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(source)
    initialize = ("init", "--quiet", *(("--object-format=" + object_format,) if object_format else ()))
    for arguments in (initialize, ("config", "core.autocrlf", "false"), ("add", "."),
                      ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                       "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "fixture")):
        subprocess.run(["git", *arguments], cwd=root, check=True, capture_output=True,
                       env=detached_git_environment())


# Beside Cargo and the pinned solver, the batch runner uses four ordinary
# POSIX tools: it captures the Kani transcript (mktemp, tee), reads it for
# CBMC's own out-of-memory sentence (grep) and removes it (rm). The fake PATH
# in the tests below is hermetic on purpose -- a solver the host happens to
# have installed must not decide a case that is about an absent one -- so the
# tools the runner may use are linked in explicitly and named here rather than
# inherited. A runner that reaches for a fifth tool fails these tests, which is
# the point: the dependency is a decision, not an accident.
RUNNER_TOOLS = ("mktemp", "tee", "grep", "rm")


def link_runner_tools(bindir: Path) -> None:
    """Put exactly the runner's declared tools on an otherwise empty PATH."""

    for tool in RUNNER_TOOLS:
        resolved = shutil.which(tool)
        if resolved is None:
            raise RuntimeError(f"the batch runner needs {tool}, which is not installed")
        if os.name == "nt":
            # A shell wrapper keeps the executable beside its MSYS runtime
            # without requiring native symlink privileges or extending PATH.
            entry = bindir / tool
            entry.write_text("#!/bin/sh\nexec " + shlex.quote(Path(resolved).as_posix()) + ' "$@"\n')
            entry.chmod(0o755)
        else:
            (bindir / tool).symlink_to(resolved)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        # S4: hooks name the caller's repository; a throwaway fixture must never
        # initialize/configure it. Explicit hook cases inject their own decoy.
        environment = mock.patch.dict(os.environ, detached_git_environment(), clear=True)
        environment.start()
        self.addCleanup(environment.stop)
        self.temporary = tempfile.TemporaryDirectory(prefix="awaken-proof-receipt-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "repo"
        prepare_source_fixture(self.root)

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, stderr=subprocess.STDOUT,
                                       env=detached_git_environment())

    def test_kani_batch_uses_the_canonical_deadline_and_driver_target_directory(self):
        # Cause/effect graph: C1 an explicit Cargo target directory and C2 a
        # user-level build.build-dir are active; C3 the configured batch deadline
        # is 37 seconds. E1 run_kani overrides only the Kani subprocess build
        # directory to <target>/kani; E2 package/jobs/harnesses remain exact; E3
        # the one shared deadline owner encloses Cargo. Constraints: the runner
        # must not grow a second timer or a per-harness rebuild path. Decision
        # rule R1: C1+C2+C3 -> E1+E2+E3.
        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        function = nested_shell_function(source, "run_kani")
        bindir = Path(self.temporary.name) / "kani-bin"
        bindir.mkdir()
        link_runner_tools(bindir)
        cargo = bindir / "cargo"
        cargo.write_text(
            '#!/bin/sh\nprintf \'%s\\n%s\\n\' "$CARGO_BUILD_BUILD_DIR" "$*" > "$CALLS"\n'
        )
        cargo.chmod(0o755)
        calls = bindir / "calls"
        deadline_call = bindir / "deadline-call"
        target = Path(self.temporary.name) / "cargo-target"
        script = (
            "set -euo pipefail\n"
            # The driver's build-directory rule moved to the cargo-target
            # authority so the batch runner and a person share one expression.
            # What this test asserts is unchanged: the fact still has to reach
            # cargo.
            f"source {shlex.quote((self.root / 'scripts/ci/_cargo_target.sh').as_posix())}\n"
            f"CARGO_TARGET_DIR={shlex.quote((target).as_posix())}\n"
            f"formal_artifact_dir={shlex.quote((bindir).as_posix())}\n"
            "kani_jobs=1\n"
            "kani_batch_timeout_seconds=37\n"
            "formal_failure_kind= formal_failure_stage= formal_failure_subject= "
            "formal_failure_deadline_seconds=\n"
            "run_with_deadline() { printf '%s\\n' \"$1\" > \"$DEADLINE_CALL\"; shift; \"$@\"; }\n"
            + function
            + "\nrun_kani awaken-dispatch --no-default-features --harness exact_claim\n"
        )
        result = run_shell_fixture(
            [bash_executable(), "-c", script],
            env=dict(os.environ, PATH=str(bindir), CALLS=calls.as_posix(), DEADLINE_CALL=deadline_call.as_posix()),
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = calls.read_text().splitlines()
        self.assertEqual(Path(lines[0]), target / "kani")
        self.assertEqual(
            lines[1],
            "kani -p awaken-dispatch --output-format terse --verbose -Z unstable-options --output-into-files -j 1 "
            "--no-default-features --harness exact_claim",
        )
        self.assertEqual(deadline_call.read_text(), "37\n")

    def test_kani_batch_outcomes_preserve_the_exact_first_failure(self):
        # Causes: C1 wrapper success, C2 ordinary Cargo failure, C3 deadline
        # status 124, C4 externally signaled Cargo status 143. Effects: E1 no
        # failure metadata or transcript, E2 exact 23 plus execution-failed,
        # E3 exact 124 plus resource-bound/package/deadline, E4 exact 143 plus
        # execution-failed. Every failure retains the batch transcript in the
        # one formal artifact directory. Decision rules R1-R4 map C1-C4 to
        # E1-E4; no later receipt step may replace the first cause.
        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        function = nested_shell_function(source, "run_kani")
        for wrapped_status, expected in (
            (0, (0, "", "", "", "")),
            (23, (23, "execution-failed", "cargo-kani", "awaken-provider", "")),
            (124, (124, "resource-bound", "cargo-kani", "awaken-provider", "37")),
            (143, (143, "execution-failed", "cargo-kani", "awaken-provider", "")),
        ):
            with self.subTest(wrapped_status=wrapped_status):
                artifact_dir = Path(self.temporary.name) / f"kani-artifacts-{wrapped_status}"
                artifact_dir.mkdir()
                script = (
                    "set -uo pipefail\nCARGO_TARGET_DIR=/build/fixture\nkani_jobs=1\n"
                    f"formal_artifact_dir={shlex.quote((artifact_dir).as_posix())}\n"
                    "kani_batch_timeout_seconds=37\n"
                    "awaken_configure_kani_driver_target() { :; }\n"
                    "formal_failure_kind= formal_failure_stage= formal_failure_subject= "
                    "formal_failure_deadline_seconds=\n"
                    f"run_with_deadline() {{ printf 'proof transcript\\n'; return {wrapped_status}; }}\n"
                    + function
                    + "\nrun_kani awaken-provider --harness exact\n"
                    "status=$?\nprintf '%s\\n' \"$status\" \"$formal_failure_kind\" "
                    "\"$formal_failure_stage\" \"$formal_failure_subject\" "
                    "\"$formal_failure_deadline_seconds\"\n"
                )
                result = run_shell_fixture([bash_executable(), "-c", script], capture_output=True,
                                        text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                lines = result.stdout.splitlines()[-5:]
                self.assertEqual(tuple([int(lines[0]), *lines[1:]]), expected)
                transcripts = list(artifact_dir.glob("kani-awaken-provider.*.log"))
                self.assertEqual(len(transcripts), 0 if wrapped_status == 0 else 1)
                if transcripts:
                    self.assertEqual(transcripts[0].read_text(), "proof transcript\n")
                    self.assertIn(transcripts[0].as_posix(), result.stderr)

    def test_kani_backend_configuration_and_memory_diagnostics_survive(self):
        # D1 a CBMC option rejection and D2 a backend memory failure each keep
        # the original exit and raw reason. The actual batch runner invokes a
        # backend port that exposes its reason only when verbose was requested;
        # removing that flag reproduces the opaque status-1 failure. Memory
        # classification consumes that same transcript, without rerunning proof.
        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        function = nested_shell_function(source, "run_kani")
        for label, diagnostic, kind in (
            ("invalid-option", "Invalid User Input\nOption: unwindset\nReason: invalid loop identifier stale_drop", "execution-failed"),
            ("memory", "CBMC appears to have run out of memory", "memory-bound"),
        ):
            with self.subTest(label=label):
                bindir = Path(self.temporary.name) / ("backend-" + label)
                bindir.mkdir()
                artifact_dir = bindir / "artifacts"
                artifact_dir.mkdir()
                link_runner_tools(bindir)
                cargo = bindir / "cargo"
                cargo.write_text("#!/bin/sh\ncase \" $* \" in *' --verbose '*) printf '%s\\n' \"$PROOF_DIAGNOSTIC\";; esac\nexit 1\n")
                cargo.chmod(0o755)
                script = (
                    "set -uo pipefail\nkani_jobs=1; kani_batch_timeout_seconds=37\n"
                    f"formal_artifact_dir={shlex.quote((artifact_dir).as_posix())}\n"
                    "formal_failure_kind= formal_failure_stage= formal_failure_subject= formal_failure_deadline_seconds=\n"
                    "awaken_configure_kani_driver_target() { :; }\n"
                    "run_with_deadline() { shift; \"$@\"; }\n" + function +
                    "\nrun_kani awaken-runtime-contract --harness resume_binding\n"
                    "status=$?; printf 'kind=%s\\n' \"$formal_failure_kind\"; exit \"$status\"\n"
                )
                result = run_shell_fixture(
                    [bash_executable(), "-c", script], capture_output=True, text=True, timeout=10,
                    env=dict(os.environ, PATH=str(bindir), PROOF_DIAGNOSTIC=diagnostic),
                )
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(diagnostic, result.stdout)
                self.assertIn("kind=" + kind, result.stdout)
                [transcript] = artifact_dir.glob("kani-awaken-runtime-contract.*.log")
                self.assertEqual(transcript.read_text(), diagnostic + "\n")
                self.assertIn(transcript.as_posix(), result.stderr)

    def test_pinned_solver_selection_and_strict_missing_tool_policy(self):
        # Z1 installed exact version + proof success => invoke the exact harness.
        # Z2 wrong/unreadable version => fail before proof. Z3 missing solver =>
        # mark missing, which fails strict mode and is explicit in diagnostic mode.
        # Z4 proof failure => propagate. Execute the real runner selection and its
        # final strict check with a solver process stub; this is not proof evidence.
        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        # The real runner decides the solver for every batch, so execute it
        # (and its pinned-version helper) on the planned Z3 batch of
        # awaken-protocol-managed, whose one harness the expectation names.
        import formal_plan
        runner = source[source.index("  run_kani() {"):source.index("  # The batches are derived, not listed")]
        [batch] = [batch for batch in formal_plan.batches(ROOT, "full")
                   if batch.selection.profile == "solver-z3-v1"
                   and batch.selection.package == "awaken-protocol-managed"]
        invocation = "  run_kani " + " ".join(batch.argv()) + "\n"
        selection = runner + invocation
        strict_check = "\nif [ \"$require_tools\"" + source.rsplit("\nif [ \"$require_tools\"", 1)[1]
        version = begin_record(self.root, True, 100, FULL_SELECTION)["proof_toolchain"]["z3"]
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
                link_runner_tools(bindir)
                if installed:
                    tool = bindir / "z3"
                    tool.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(reported) + f"\nexit {version_exit}\n")
                    tool.chmod(0o755)
                calls = bindir / "calls"
                script = f"set -euo pipefail\nmissing=0\nrequire_tools={strict}\nz3_version={shlex.quote(version)}\n"
                script += f"formal_artifact_dir={shlex.quote((bindir).as_posix())}\n"
                script += "kani_jobs=1\nkani_batch_timeout_seconds=1\nawaken_configure_kani_driver_target() { :; }\n"
                script += "run_with_deadline() { shift; printf '%s\\n' \"$*\" > \"$CALLS\"; return \"$PROOF_EXIT\"; }\n"
                result = run_shell_fixture([bash_executable(), "-c", script + selection + strict_check],
                                        env=dict(os.environ, PATH=str(bindir), CALLS=calls.as_posix(), PROOF_EXIT=str(proof_exit)),
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, expected, result.stderr)
                invoked = installed and reported == version and version_exit == 0
                self.assertEqual(calls.exists(), invoked)
                if invoked:
                    self.assertEqual(calls.read_text().strip(), "cargo kani -p awaken-protocol-managed --output-format terse --verbose -Z unstable-options --output-into-files -j 1 --solver z3 --exact --harness "
                                     "resources::memory_stores::memory_time_filter_compares_visible_milliseconds_without_merging_them")
                if not installed:
                    self.assertIn("skipped Z3", result.stdout)
        passed = finish_record(begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110)
        changed = dict(passed, proof_toolchain=dict(passed["proof_toolchain"], z3="wrong"))
        with self.assertRaisesRegex(ValueError, "toolchain"):
            validate_record(changed, self.root, 110)

    def test_external_sat_solver_is_found_before_proof_or_named_missing(self):
        # S1 kissat on PATH -> the batch runs; S2 kissat absent -> no proof
        # runs, the batch is named skipped, strict mode fails and diagnostic
        # mode completes; S3 a solver compiled into CBMC (cadical) needs no
        # binary. Absent kissat used to read as "0 of N failed ... FAILED".
        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        runner = source[source.index("  run_kani() {"):source.index("  # The batches are derived, not listed")]
        strict_check = "\nif [ \"$require_tools\"" + source.rsplit("\nif [ \"$require_tools\"", 1)[1]
        for solver, installed, strict, expected, invoked in (
            ("kissat", True, 1, 0, True),
            ("kissat", False, 1, 1, False),
            ("kissat", False, 0, 0, False),
            ("cadical", False, 1, 0, True),
        ):
            with self.subTest(solver=solver, installed=installed, strict=strict), tempfile.TemporaryDirectory() as temporary:
                bindir = Path(temporary)
                link_runner_tools(bindir)
                if installed:
                    (bindir / solver).write_text("#!/bin/sh\nexit 0\n")
                    (bindir / solver).chmod(0o755)
                calls = bindir / "calls"
                script = (f"set -euo pipefail\nmissing=0\nrequire_tools={strict}\nz3_version=unused\n"
                          f"formal_artifact_dir={shlex.quote((bindir).as_posix())}\n"
                          "kani_solver_tools=/released/kani/bin\nkani_jobs=1\nkani_batch_timeout_seconds=1\n"
                          "awaken_configure_kani_driver_target() { :; }\n"
                          "run_with_deadline() { shift; printf '%s\\n' \"$*\" > \"$CALLS\"; }\n")
                invocation = f"  run_kani awaken-protocol-managed --harness a --solver {solver} --harness b\n"
                result = run_shell_fixture([bash_executable(), "-c", script + runner + invocation + strict_check],
                                        env=dict(os.environ, PATH=str(bindir), CALLS=calls.as_posix()),
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                self.assertEqual(calls.exists(), invoked)
                if not installed and solver == "kissat":
                    self.assertIn("skipped kissat harnesses of awaken-protocol-managed", result.stdout)
                    self.assertIn("/released/kani/bin", result.stdout)

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
        record = begin_record(self.root, True, 100, FULL_SELECTION)
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

    def test_nonpassing_receipt_classifies_resource_bounds_without_promotion(self):
        # Causes: C1 ordinary execution exits 23; C2 a named Kani package reaches
        # the canonical 124 deadline; C3 resource-bound metadata is incomplete or
        # attached to another exit code. Effects: E1/E2 become Not-passed with the
        # exact first exit and classification; neither validates for release; E3
        # is rejected. Constraints: only a strict unchanged exit-0 invocation may
        # become Passed. Decision rules: R1=C1->N1, R2=C2->N2, R3=C3->reject.
        ordinary = finish_record(
            begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110,
            23, "execution-failed", "formal-gate",
        )
        self.assertEqual(ordinary["status"], "not-passed")
        self.assertEqual(ordinary["outcome"], {
            "kind": "execution-failed", "exit_code": 23, "stage": "formal-gate",
        })
        timed_out = finish_record(
            begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110,
            124, "resource-bound", "cargo-kani", "awaken-provider-genai", 37,
        )
        self.assertEqual(timed_out["status"], "not-passed")
        self.assertEqual(timed_out["outcome"], {
            "kind": "resource-bound", "exit_code": 124, "stage": "cargo-kani",
            "subject": "awaken-provider-genai", "deadline_seconds": 37,
        })
        for record in (ordinary, timed_out):
            with self.assertRaisesRegex(ValueError, "not a completed strict run"):
                validate_record(record, self.root, 110)
        for exit_code, subject, deadline in ((23, "package", 37), (124, None, 37),
                                             (124, "package", 0)):
            with self.subTest(exit_code=exit_code, subject=subject, deadline=deadline), \
                    self.assertRaises(ValueError):
                finish_record(
                    begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110,
                    exit_code, "resource-bound", "cargo-kani", subject, deadline,
                )

    def test_memory_bound_failure_seals_without_a_deadline_or_release_promotion(self):
        # M1 backend exit 1 or an OOM exit 137 with an exact package seals the
        # original non-passing cause. M2 an absent/empty subject, a deadline
        # exit, or a claimed deadline is rejected. M3 the actual CLI accepts the
        # same kind as the runner/API and writes Not-passed, never Running or
        # Passed. Release validation rejects every successful sealing result.
        for exit_code in (1, 137):
            with self.subTest(exit_code=exit_code):
                failed = finish_record(
                    begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110,
                    exit_code, "memory-bound", "cargo-kani", "awaken-runtime-contract",
                )
                self.assertEqual(failed["status"], "not-passed")
                self.assertTrue(failed["source_unchanged"])
                self.assertEqual(failed["outcome"], {
                    "kind": "memory-bound", "exit_code": exit_code,
                    "stage": "cargo-kani", "subject": "awaken-runtime-contract",
                })
                with self.assertRaisesRegex(ValueError, "not a completed strict run"):
                    validate_record(failed, self.root, 110)
        for exit_code, subject, deadline in ((1, None, None), (1, "", None),
                                             (124, "package", None), (1, "package", 37)):
            with self.subTest(exit_code=exit_code, subject=subject, deadline=deadline), \
                    self.assertRaises(ValueError):
                finish_record(
                    begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110,
                    exit_code, "memory-bound", "cargo-kani", subject, deadline,
                )
        record_path = self.root / "target/memory-bound.json"
        write_record(record_path, begin_record(self.root, True, 100, FULL_SELECTION))
        result = subprocess.run([
            sys.executable, str(self.root / "scripts/ci/formal_run_evidence.py"), "finish",
            "--record", str(record_path), "--exit-code", "1", "--failure-kind", "memory-bound",
            "--failure-stage", "cargo-kani", "--failure-subject", "awaken-runtime-contract",
        ], cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        sealed = json.loads(record_path.read_text())
        self.assertEqual(sealed["status"], "not-passed")
        self.assertEqual(sealed["outcome"]["kind"], "memory-bound")
        with self.assertRaisesRegex(ValueError, "not a completed strict run"):
            validate_record(sealed, self.root)

    def test_every_release_identity_axis_is_binding(self):
        # V2-V4: take a valid completed receipt, change one axis at a time.
        # Every mutation rejects independently; the positive row prevents a
        # reject-everything implementation from satisfying this decision table.
        passed = finish_record(begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110)
        validate_record(passed, self.root, 110)
        # V5 each new CBMC source/patch/options axis comes from bootstrap and
        # independently invalidates a receipt when changed; no second pin list.
        backend_axes = ("cbmc_source_revision", "cbmc_source_patch_sha256", "cbmc_build_options")
        for axis in backend_axes:
            self.assertTrue(passed["proof_toolchain"][axis])
        mutations = tuple(
            ("proof_toolchain", dict(passed["proof_toolchain"], **{axis: "wrong"}))
            for axis in backend_axes
        ) + (
            ("status", "running"), ("strict", False), ("schema_version", 0),
            ("run_id", ""), ("started_at_unix", 120), ("finished_at_unix", 111),
            ("source", dict(passed["source"], commit="a" * 40)),
            ("source", dict(passed["source"], tree="a" * 40)),
            ("source", dict(passed["source"], clean=False)),
            ("proof_toolchain", {}), ("runner", dict(passed["runner"], digest="sha256:" + "0" * 64)),
            # Tiering: a pr or narrowed run, or a receipt naming another plan,
            # cannot qualify a release even when everything else matches.
            ("selection", {"tier": "pr", "affected_base": None}),
            ("selection", {"tier": "pr", "affected_base": "HEAD"}),
            ("plan_digest", "sha256:" + "0" * 64), ("schema_version", 1),
        )
        for key, value in mutations:
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                validate_record(dict(passed, **{key: value}), self.root, 110)
        # A pr receipt is a real passed receipt of its own plan, not of the full one.
        pr = finish_record(begin_record(self.root, True, 100, {"tier": "pr", "affected_base": None}),
                           self.root, False, 110)
        self.assertEqual(pr["status"], "passed")
        with self.assertRaisesRegex(ValueError, "not a full-tier run"):
            validate_record(pr, self.root, 110)
        with self.assertRaisesRegex(ValueError, "only for a pr run"):
            begin_record(self.root, True, 100, {"tier": "full", "affected_base": "HEAD"})

    def test_new_invocation_retires_old_success_and_atomic_file_is_valid(self):
        # C1 previous Passed; C2 new Running. E1 the same output path no longer
        # validates until the new invocation completes. No second report store.
        path = self.root / "target/formal-run.json"
        first = begin_record(self.root, True, 100, FULL_SELECTION)
        write_record(path, finish_record(first, self.root, False, 110))
        validate_record(json.loads(path.read_text()), self.root, 110)
        second = begin_record(self.root, True, 120, FULL_SELECTION)
        self.assertNotEqual(first["run_id"], second["run_id"])
        write_record(path, second)
        with self.assertRaises(ValueError):
            validate_record(json.loads(path.read_text()), self.root, 120)
        self.assertEqual([p.name for p in path.parent.iterdir()], [path.name])

    def test_actual_gate_failure_replaces_stale_success_with_not_passed(self):
        # C1 a completed previous run; C2 the actual gate starts. R1 its first
        # checker fails (23); R2 bootstrap --paths rejects a changed patch (1),
        # before any checker. Both must retire stale Passed, preserve the exact
        # failure and seal diagnostics. Budget resolution and receipt writing
        # are setup calls, not proof checkers. R2 guards initialization order:
        # tool selection stays inside the existing receipt/EXIT lifecycle.
        path = self.root / "target/test-timings/formal-run.json"
        bindir = Path(self.temporary.name) / "bin"
        bindir.mkdir()
        checker_started = bindir / "checker-started"
        shim = bindir / "python3"
        shim.write_text("#!/bin/sh\ncase \"$1\" in\n"
                        "  scripts/ci/formal_run_evidence.py|scripts/ci/formal_budget.py) exec " + shlex.quote(Path(sys.executable).as_posix()) + " \"$@\" ;;\n"
                        "  *) touch \"$CHECKER_STARTED\"; exit 23 ;;\nesac\n")
        shim.chmod(0o755)
        env = dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                   AWAKEN_FORMAL_RUN_EVIDENCE=path.as_posix(), CHECKER_STARTED=checker_started.as_posix())
        patch = self.root / "scripts/ci/patches/cbmc-smt-type-dependencies.patch"
        original = patch.read_bytes()
        for stage, expected in (("checker", 23), ("tool-selection", 1)):
            with self.subTest(stage=stage):
                shutil.rmtree(self.root / "target", ignore_errors=True)
                checker_started.unlink(missing_ok=True)
                if stage == "tool-selection":
                    patch.write_bytes(original + b"\n")
                write_record(path, finish_record(begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110))
                result = run_shell_fixture([bash_executable(), "scripts/ci/check_formal.sh", "--require-tools"],
                                        cwd=self.root, env=env, capture_output=True, text=True, timeout=30)
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(checker_started.exists(), stage == "checker")
                record = json.loads(path.read_text())
                self.assertEqual(record["status"], "not-passed")
                self.assertEqual(record["outcome"], {
                    "kind": "execution-failed", "exit_code": expected, "stage": "formal-gate",
                })
                bundles = list((self.root / "target/test-timings").glob("formal-*/evidence.json"))
                self.assertEqual(len(bundles), 1)
                failure = verify_artifacts(bundles[0].parent, self.root)
                self.assertEqual((failure["status"], failure["exit_code"]), ("failed", expected))
                patch.write_bytes(original)

    def test_invalid_kani_deadline_fails_before_the_first_gate_command(self):
        # Causes: C1 the explicit Kani deadline is zero; C2 it exceeds the
        # canonical ceiling. Effects: E1 Running becomes Not-passed with exact
        # configuration subject/exit 2; E2 the first checker is not invoked;
        # E3 the diagnostic bundle seals as failed. Rules C1=>E1-E3 and
        # C2=>E1-E3. The budget owner executes after receipt/trap setup.
        bindir = Path(self.temporary.name) / "invalid-config-bin"
        bindir.mkdir()
        checker_started = bindir / "checker-started"
        shim = bindir / "python3"
        shim.write_text(
            "#!/bin/sh\ncase \"$1\" in\n"
            "  scripts/ci/formal_run_evidence.py|scripts/ci/formal_budget.py) exec "
            + shlex.quote(Path(sys.executable).as_posix()) + " \"$@\" ;;\n"
            "  *) touch \"$CHECKER_STARTED\"; exit 0 ;;\n"
            "esac\n"
        )
        shim.chmod(0o755)
        path = self.root / "target/test-timings/formal-run.json"
        for override in ("0", str(formal_budget.KANI_BATCH_TIMEOUT_CEILING_SECONDS + 1)):
            with self.subTest(override=override):
                shutil.rmtree(self.root / "target", ignore_errors=True)
                checker_started.unlink(missing_ok=True)
                result = run_shell_fixture(
                    [bash_executable(), "scripts/ci/check_formal.sh", "--require-tools"],
                    cwd=self.root,
                    env=dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                             AWAKEN_FORMAL_RUN_EVIDENCE=path.as_posix(),
                             AWAKEN_KANI_BATCH_TIMEOUT_SECONDS=override,
                             CHECKER_STARTED=checker_started.as_posix()),
                    capture_output=True, text=True, timeout=30,
                )
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertFalse(checker_started.exists())
                record = json.loads(path.read_text())
                self.assertEqual(record["status"], "not-passed")
                self.assertEqual(record["outcome"], {
                    "kind": "execution-failed", "exit_code": 2,
                    "stage": "cargo-kani-configuration",
                    "subject": "AWAKEN_KANI_BATCH_TIMEOUT_SECONDS",
                })
                bundles = list((self.root / "target/test-timings").glob("formal-*/evidence.json"))
                self.assertEqual(len(bundles), 1)
                self.assertEqual(verify_artifacts(bundles[0].parent, self.root)["exit_code"], 2)

    def test_actual_timeout_trap_preserves_resource_bound_if_artifact_sealing_fails(self):
        # Causes: C1 the Kani boundary records package/deadline then returns 124;
        # C2 artifact sealing independently fails. Effects: E1 process exit remains
        # the first 124; E2 the sole formal receipt remains Not-passed and names
        # the resource boundary; E3 no Passed publication occurs. Constraints:
        # diagnostic artifact completeness cannot rewrite proof execution truth.
        # Decision rule R1=C1+C2->E1+E2+E3.
        initialization = formal_gate_initialization(self.root)
        bindir = Path(self.temporary.name) / "timeout-bin"
        bindir.mkdir()
        shim = bindir / "python3"
        shim.write_text(
            '#!/bin/sh\nif [ "$2" = artifacts-finish ]; then exit 37; fi\nexec '
            + shlex.quote(Path(sys.executable).as_posix()) + ' "$@"\n'
        )
        shim.chmod(0o755)
        path = self.root / "target/test-timings/formal-run.json"
        body = initialization + (
            "\nformal_failure_kind=resource-bound\n"
            "formal_failure_stage=cargo-kani\n"
            "formal_failure_subject=awaken-provider-genai\n"
            "formal_failure_deadline_seconds=37\nexit 124\n"
        )
        result = run_shell_fixture(
            [bash_executable(), "-c", body, "scripts/ci/check_formal.sh", "--require-tools"],
            cwd=self.root,
            env=dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                     AWAKEN_FORMAL_RUN_EVIDENCE=path.as_posix()),
            capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 124, result.stderr)
        record = json.loads(path.read_text())
        self.assertEqual(record["status"], "not-passed")
        self.assertEqual(record["outcome"], {
            "kind": "resource-bound", "exit_code": 124, "stage": "cargo-kani",
            "subject": "awaken-provider-genai", "deadline_seconds": 37,
        })

    def test_cleanup_failure_never_replaces_the_body_failure(self):
        # Causes: C1 body exits 23 or C2 a named Kani boundary exits 124; C3 the
        # temporary-directory cleanup returns 41 afterward. Effects: E1 exact
        # first status survives; E2 its Not-passed outcome survives; E3 cleanup
        # failure is diagnostic only. Constraints: cleanup cannot promote or
        # rewrite verification evidence. Rules R1=C1+C3->23, R2=C2+C3->124.
        # Inject the cleanup command port explicitly. Native shell startup
        # may resolve its own rm before an extensionless PATH fixture, which
        # leaves the intended cleanup failure unexercised.
        initialization = ('rm() { echo cleanup-failure >&2; return 41; }\n'
                          + formal_gate_initialization(self.root))
        for body_status in (23, 124):
            with self.subTest(body_status=body_status):
                path = self.root / f"target/test-timings/formal-run-{body_status}.json"
                metadata = ""
                if body_status == 124:
                    metadata = (
                        "formal_failure_kind=resource-bound\n"
                        "formal_failure_stage=cargo-kani\n"
                        "formal_failure_subject=awaken-provider-genai\n"
                        "formal_failure_deadline_seconds=37\n"
                    )
                result = run_shell_fixture(
                    [bash_executable(), "-c", initialization + "\n" + metadata
                     + f"exit {body_status}\n",
                     "scripts/ci/check_formal.sh", "--require-tools"],
                    cwd=self.root,
                    env=dict(os.environ, AWAKEN_FORMAL_RUN_EVIDENCE=path.as_posix()),
                    capture_output=True, text=True, timeout=30,
                )
                self.assertEqual(result.returncode, body_status, result.stderr)
                self.assertIn("cleanup-failure", result.stderr)
                self.assertIn("failed to clean formal temporary directory", result.stderr)
                record = json.loads(path.read_text())
                self.assertEqual(record["status"], "not-passed")
                self.assertEqual(record["outcome"]["exit_code"], body_status)

    def test_actual_completion_trap_seals_before_publishing_success(self):
        # Causes: C1 body and artifact sealing succeed; C2 body succeeds but
        # artifact sealing exits 37. Effects: E1 strict Passed; E2 exact 37 plus
        # Not-passed/artifact-finalization. Constraints: diagnostic failure can
        # never leave or publish success. Rules R1=C1->E1, R2=C2->E2. Execute the
        # real initialization/EXIT trap while replacing only the proof body.
        # Cooperative metadata/fixture work must precede the heavy Java PATH
        # adapter. Otherwise its admission diagnostics replace the TLC version
        # before these original success/seal-failure effects can be exercised.
        initialization = formal_gate_initialization(self.root)
        bindir = Path(self.temporary.name) / "completion-bin"
        bindir.mkdir()
        shim = bindir / "python3"
        shim.write_text('#!/bin/sh\nif [ "$2" = artifacts-finish ] && [ "$FAIL_SEAL" = 1 ]; then exit 37; fi\nexec '
                        + shlex.quote(Path(sys.executable).as_posix()) + ' "$@"\n')
        shim.chmod(0o755)
        path = self.root / "target/test-timings/formal-run.json"
        for fail_seal in (False, True):
            with self.subTest(fail_seal=fail_seal):
                env = dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                           FAIL_SEAL=str(int(fail_seal)), AWAKEN_FORMAL_RUN_EVIDENCE=path.as_posix())
                result = run_shell_fixture([bash_executable(), "-c", initialization + "\nformal_body_complete=1\nexit 0\n",
                                         "scripts/ci/check_formal.sh", "--require-tools"],
                                        cwd=self.root, env=env, capture_output=True, text=True, timeout=30)
                self.assertEqual(result.returncode, 37 if fail_seal else 0, result.stderr)
                record = json.loads(path.read_text())
                self.assertEqual(record["status"], "not-passed" if fail_seal else "passed")
                if fail_seal:
                    self.assertEqual(record["outcome"], {
                        "kind": "execution-failed", "exit_code": 37,
                        "stage": "artifact-finalization", "subject": "formal",
                    })

    @unittest.skipIf(os.name == "nt", "cooperative host admission is Linux; native deadline tests own Windows")
    def test_cooperative_java_adapter_follows_framework_and_keeps_heavy_admission(self):
        # Causes: C1 cooperative phase versus an inherited exclusive admission;
        # C2 the existing fake host port admits (0) or refuses (75). Effects:
        # E1 metadata/receipt initialization reads the exact pinned version with
        # no admission; E2 only after framework checks does the SAME adapter
        # admit heavy Java; E3 refusal preserves 75 and executes no Java body;
        # E4 an inherited admission executes directly, without reacquiring it.
        # Rules: phase+0=>E1/E2; phase+75=>E1/E3; inherited+75=>E1/E4.
        # The Java body is an explicit recording port, not a model or proof.
        # FullGateAdmissionTests owns the fake Cargo/admission protocol; execute
        # the real gate prefix, original adapter bytes and shared deadline here.
        from test_full_gate_admission import FullGateAdmissionTests

        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        marker = 'if [ "${AWAKEN_ADMISSION_PHASE_MODE:-}" = 1 ]; then\n'
        self.assertEqual(source.count(marker), 1)
        start = source.index(marker)
        block = source[start:source.index("\nfi\n", start) + len("\nfi\n")]
        self.assertGreater(start, source.index("python3 scripts/ci/check_formal_mutations.py\n"))
        self.assertLess(start, source.index("run_required_rust_tests -p awaken-service-lifecycle"))
        initialization = formal_gate_initialization(self.root)
        self.assertNotIn(marker, initialization)
        for inherited, status in ((False, 0), (False, 75), (True, 75)):
            with self.subTest(inherited=inherited, admission=status), tempfile.TemporaryDirectory() as tmp:
                fixture = Path(tmp)
                host, env = FullGateAdmissionTests().fixture(fixture)
                bindir = fixture / "recording-java"
                bindir.mkdir()
                java = bindir / "java"
                java.write_text(
                    '#!/bin/sh\nif [ "${4:-}" = -version ]; then\n'
                    " printf '%s\\n' " + shlex.quote(proof_toolchain(self.root)["tlc"]) + "; exit 0\nfi\n"
                    '[ "${AWAKEN_CARGO_ADMITTED:-}" = 1 ] || exit 95\n'
                    'printf "executed\\n" > "$JAVA_BODY"\nexit 23\n'
                )
                java.chmod(0o755)
                jar = fixture / "fixture.jar"
                jar.touch()
                env.update(PATH=f"{bindir}:{host}:{env['PATH']}",
                           AWAKEN_ADMISSION_PHASE_MODE="1", FIXTURE_STATUS=str(status),
                           TLA2TOOLS_JAR=jar.as_posix(), JAVA_BODY=(fixture / "body").as_posix(),
                           METADATA_READY=(fixture / "metadata-ready").as_posix(),
                           AWAKEN_FORMAL_RUN_EVIDENCE=(fixture / "receipt.json").as_posix())
                if inherited:
                    env["AWAKEN_CARGO_ADMITTED"] = "1"
                body = (initialization + '\nprintf "ready\\n" > "$METADATA_READY"\n'
                        + block + '\njava owned-heavy-proof-port\n')
                result = run_shell_fixture(
                    [bash_executable(), "-c", body, "scripts/ci/check_formal.sh", "--require-tools"],
                    cwd=self.root, env=env, capture_output=True, text=True, timeout=30,
                )
                self.assertTrue((fixture / "metadata-ready").exists(), result.stderr)
                self.assertEqual(result.returncode, 75 if status == 75 and not inherited else 23,
                                 result.stderr)
                self.assertEqual((fixture / "body").exists(), status == 0 or inherited)
                self.assertFalse((fixture / "wrong-cargo-ran").exists())
                self.assertEqual(list(fixture.glob("awaken-cargo-deadline.*")), [])

    def test_incomplete_or_interrupted_body_never_publishes_passed(self):
        # Receipt cause/effect table: C1 the formal body exits zero before its
        # last check; C2 TERM interrupts an in-progress body. E1 neither can
        # publish Passed; E2 the one diagnostic bundle is sealed Failed with
        # the same nonzero exit. R1=C1 -> exit 1 + E1/E2; R2=C2 -> exit 143
        # + E1/E2. These execute the real gate initialization and EXIT owner,
        # without claiming that a fixture has run any proof tool.
        initialization = formal_gate_initialization(self.root)
        path = self.root / "target/test-timings/formal-run.json"
        for label, body, expected_exit in (
            ("early-zero", "exit 0", 1),
            ("term", "kill -TERM $$", 143),
        ):
            with self.subTest(label=label):
                shutil.rmtree(self.root / "target", ignore_errors=True)
                result = run_shell_fixture(
                    [bash_executable(), "-c", initialization + "\n" + body + "\n",
                     "scripts/ci/check_formal.sh", "--require-tools"],
                    cwd=self.root,
                    env=dict(os.environ, AWAKEN_FORMAL_RUN_EVIDENCE=path.as_posix()),
                    capture_output=True, text=True, timeout=30,
                )
                self.assertEqual(result.returncode, expected_exit, result.stderr)
                record = json.loads(path.read_text())
                self.assertEqual(record["status"], "not-passed")
                self.assertEqual(record["outcome"]["exit_code"], expected_exit)
                bundles = list((self.root / "target/test-timings").glob("formal-*/evidence.json"))
                self.assertEqual(len(bundles), 1)
                failure = verify_artifacts(bundles[0].parent, self.root)
                self.assertEqual((failure["status"], failure["exit_code"]), ("failed", expected_exit))

    @unittest.skipUnless(hasattr(os, "killpg"), "POSIX process-group signaling; native Job termination is covered by deadline tests")
    def test_external_group_termination_cannot_certify_partial_formal_run(self):
        # C1 the receipt and EXIT owner are installed; C2 the active child has
        # installed its TERM disposition before an external TERM reaches the
        # gate and child as one process group before the last proof; C3 the
        # host may concurrently compile other workspaces.
        # E1 the process exits 143, E2 the exact clean source gets Not-passed,
        # E3 its diagnostic bundle seals Failed within the same 30-second
        # fixture deadline used by neighboring process tests. R1=C1+C2->E1-E3
        # for either value of C3.
        # This is the real interruption shape that once left a strict Passed
        # JSON after only manifest self-tests. The child itself emits `ready`
        # after installing its TERM disposition, so a signal cannot race with
        # its fork/exec boundary; no proof is simulated Passed.
        initialization = formal_gate_initialization(self.root)
        path = self.root / "target/test-timings/formal-run.json"
        child_script = (
            "import signal, time; "
            "signal.signal(signal.SIGTERM, signal.SIG_DFL); "
            "print('ready', flush=True); time.sleep(30)"
        )
        child_command = (
            f"{shlex.quote(Path(sys.executable).as_posix())} -u -c {shlex.quote(child_script)} "
            '& child=$!\nwait "$child"\n'
        )
        process = subprocess.Popen(
            [bash_executable(), "-c", initialization + "\n" + child_command,
             "scripts/ci/check_formal.sh", "--require-tools"],
            cwd=self.root,
            env=dict(os.environ, AWAKEN_FORMAL_RUN_EVIDENCE=path.as_posix()),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            ready = b""
            deadline = time.monotonic() + 10
            while b"ready\n" not in ready:
                remaining = deadline - time.monotonic()
                self.assertGreater(remaining, 0, "formal fixture never installed its EXIT owner")
                self.assertTrue(select.select([process.stdout], [], [], remaining)[0], "formal fixture readiness timed out")
                chunk = os.read(process.stdout.fileno(), 4096)
                self.assertTrue(chunk, "formal fixture exited before readiness")
                ready += chunk
            os.killpg(process.pid, signal.SIGTERM)
            _, stderr = process.communicate(timeout=30)
            self.assertEqual(process.returncode, 143, stderr.decode())
            record = json.loads(path.read_text())
            self.assertEqual(record["status"], "not-passed")
            self.assertEqual(record["outcome"]["exit_code"], 143)
            bundles = list((self.root / "target/test-timings").glob("formal-*/evidence.json"))
            self.assertEqual(len(bundles), 1)
            failure = verify_artifacts(bundles[0].parent, self.root)
            self.assertEqual((failure["status"], failure["exit_code"]), ("failed", 143))
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate(timeout=10)

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
        pnpm.write_text('#!/bin/sh\ntouch "' + (self.root / "unexpected-pnpm").as_posix() + '"\nexit 0\n')
        pnpm.chmod(0o755)
        env = dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"])
        env.pop("AWAKEN_SESSION_EVIDENCE_DIR", None)
        result = run_shell_fixture([bash_executable(), "scripts/ci/check-reliability.sh", "--session-observations"],
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
        record = finish_record(begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110)
        with self.assertRaisesRegex(ValueError, "clean release source"):
            validate_record(record, self.root, 110)

    def test_dirty_mutation_cannot_hide_behind_the_same_status_and_working_patch(self):
        # S5/S7: cross already-dirty untracked same-path bytes with an index
        # change hidden by WT restoration (same MM status, empty HEAD->WT).
        # Old formal before/after snapshots called this unchanged. Real Git and
        # artifact lifecycle must now yield Failed/source_unchanged=false.
        for cause in ("untracked", "index"):
            with self.subTest(cause=cause):
                path = self.root / ("untracked.rs" if cause == "untracked" else "transitive.rs")
                original = path.read_bytes() if path.exists() else None
                path.write_bytes(b"first\n")
                if cause == "index":
                    self.git("add", path.as_posix())
                    path.write_bytes(original)
                before = source_snapshot(self.root)
                directory = begin_artifacts(self.root, self.root / "target/evidence", "formal")
                path.write_bytes(b"second\n")
                if cause == "index":
                    self.git("add", path.as_posix())
                    path.write_bytes(original)
                after = source_snapshot(self.root)
                self.assertEqual(before["status_digest"], after["status_digest"], "the hidden axis actually stayed hidden")
                record = finish_artifacts(directory, self.root, 0)
                self.assertEqual((record["status"], record["source_unchanged"]), ("failed", False))
                self.assertNotEqual(before["working_diff_digest"], after["working_diff_digest"])
                if original is None:
                    path.unlink()
                else:
                    path.write_bytes(original)
                    self.git("add", path.as_posix())

    def test_old_weak_dirty_artifact_is_rejected_without_a_compatibility_fallback(self):
        # S11: schema1 stays unchanged, but an old dirty artifact bound only
        # HEAD->WT bytes. Keeping that old digest cannot qualify the stronger
        # dirty source on this code; fail closed rather than trying both bases.
        (self.root / "untracked.rs").write_bytes(b"old dirty bytes")
        directory = begin_artifacts(self.root, self.root / "target/evidence", "formal")
        record = finish_artifacts(directory, self.root, 0)
        record["source"]["working_diff_digest"] = digest_bytes(self.git("diff", "--binary", "HEAD"))
        write_record(directory / "evidence.json", record)
        with self.assertRaisesRegex(ValueError, "source mismatch"):
            verify_artifacts(directory, self.root)

    def test_shared_capture_preserves_partial_index_and_detaches_foreign_hooks(self):
        # S2/S3/S4: same-repo alternate index is truth; a foreign real decoy
        # cannot supply source/blob/commit, nor be mutated by fixture creation.
        # Check actual config/index/HEAD/refs/WT bytes, not just return status.
        import rust_test_plan as planner
        with mock.patch.object(sys, "path", [str(ROOT / "scripts/release"), *sys.path]):
            import verification_manifest
        original = (self.root / "transitive.rs").read_bytes()
        (self.root / "transitive.rs").write_bytes(b"staged\n")
        self.git("add", ".")
        (self.root / "transitive.rs").write_bytes(original)
        partial = self.root / ".git/partial-index"
        subprocess.run(["git", "read-tree", "HEAD"], cwd=self.root, check=True,
                       env={**detached_git_environment(), "GIT_INDEX_FILE": str(partial)})
        with mock.patch.object(planner, "ROOT", self.root):
            default = planner.source_snapshot()[0]
            expected = source_snapshot(self.root)
            for work_tree, git_dir in ((str(self.root), str(self.root / ".git")), (".", ".git"), ("../repo", "../repo/.git")):
                # Git resolves relative WORK_TREE after -C root, independently
                # of this test process's actual checkout cwd. Use Git as oracle.
                hook = dict(detached_git_environment(), GIT_DIR=git_dir,
                            GIT_INDEX_FILE=str(partial), GIT_WORK_TREE=work_tree)
                self.assertEqual(subprocess.check_output(["git", "-C", str(self.root), "status", "--porcelain=v1"], env=hook), b"")
                with mock.patch.dict(os.environ, hook, clear=True):
                    self.assertNotEqual(planner.source_snapshot()[0], default)
                    self.assertTrue(source_snapshot(self.root)["clean"], "partial matches HEAD and WT")
        decoy = self.root.parent / "decoy"
        prepare_source_fixture(decoy, {"transitive.rs": "foreign source\n"})
        def protected():
            paths = [decoy / ".git" / n for n in ("config", "index", "HEAD")]
            paths += [p for p in (decoy / ".git/refs").rglob("*") if p.is_file()]
            paths += [p for p in decoy.rglob("*") if p.is_file() and ".git" not in p.relative_to(decoy).parts]
            return {p.relative_to(decoy).as_posix(): p.read_bytes() for p in paths}
        before = protected()
        for case, directory in enumerate((str(decoy / ".git"), str(self.root / ".git"), "../decoy/.git")):
            # Also cross a matching GIT_DIR with a foreign WORK_TREE; matching
            # only the object directory must not admit another working source.
            with mock.patch.dict(os.environ, GIT_DIR=directory, GIT_INDEX_FILE=str(decoy / ".git/index"),
                                 GIT_WORK_TREE=str(decoy), GIT_PREFIX="hook/"):
                prepare_source_fixture(self.root.parent / f"from-hook-{case}")
                with mock.patch.object(planner, "ROOT", self.root):
                    self.assertEqual(planner.source_snapshot()[0], default)
                self.assertEqual(source_snapshot(self.root), expected)
                self.assertEqual(committed_file_digest(self.root, "transitive.rs"), digest_bytes(original))
                self.assertEqual(verification_manifest.source_commit(self.root), expected["commit"])
        self.assertEqual(protected(), before)

    def test_unmerged_diagnostics_and_invalid_roots_keep_their_distinct_boundaries(self):
        # S8: conflict stages still bind formal dirty diagnostics, while working
        # read-only tree admission refuses. Git128 and a discovered parent's
        # unadmitted subdirectory are distinct failures for every consumer.
        # The temp directory itself may be inside a caller's Git work tree.
        import rust_test_plan as planner
        with mock.patch.object(sys, "path", [str(ROOT / "scripts/release"), *sys.path]):
            import verification_manifest
        blob = self.git("hash-object", "transitive.rs").decode().strip()
        subprocess.run(["git", "update-index", "--index-info"], cwd=self.root, check=True,
                       input=f"100644 {blob} 1\ttransitive.rs\n".encode(), env=detached_git_environment())
        before = source_snapshot(self.root)
        changed = subprocess.check_output(["git", "hash-object", "-w", "--stdin"], cwd=self.root,
                                          input=b"another conflict object", env=detached_git_environment()).decode().strip()
        subprocess.run(["git", "update-index", "--index-info"], cwd=self.root, check=True,
                       input=f"100644 {changed} 1\ttransitive.rs\n".encode(), env=detached_git_environment())
        after = source_snapshot(self.root)
        self.assertFalse(after["clean"])
        self.assertEqual(before["status_digest"], after["status_digest"])
        self.assertNotEqual(before["working_diff_digest"], after["working_diff_digest"])
        with mock.patch.object(planner, "ROOT", self.root), self.assertRaisesRegex(formal_sources.SourceError, "unmerged index"):
            planner.source_snapshot()
        outside = self.root.parent / "outside"
        outside.mkdir()
        discovered = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], cwd=outside,
            env=detached_git_environment(), capture_output=True, text=True,
        )
        # Discover the fixture's real topology independently of the source
        # adapter. Accept neither an arbitrary exception nor an arbitrary Git
        # status: this location is a parent-owned subdirectory or has no repo.
        self.assertIn(discovered.returncode, (0, 128))
        outside_error = subprocess.CalledProcessError
        if discovered.returncode == 0:
            self.assertNotEqual(Path(discovered.stdout.strip()).resolve(), outside.resolve())
            outside_error = formal_sources.SourceError
        # A bare repository deterministically has no working-tree top, even
        # under a caller's checkout. Keep real Git128 coverage on every host.
        bare = self.root.parent / "bare"
        subprocess.run(["git", "init", "--quiet", "--bare", str(bare)],
                       env=detached_git_environment(), check=True, capture_output=True)
        for relative, error in (("outside", outside_error), ("bare", subprocess.CalledProcessError),
                                ("repo/subdirectory", formal_sources.SourceError)):
            directory = self.root.parent / relative
            directory.mkdir(exist_ok=True)
            with mock.patch.object(planner, "ROOT", directory):
                for consumer, invoke in (
                    ("formal", lambda: source_snapshot(directory)),
                    ("blob", lambda: committed_file_digest(directory, "transitive.rs")),
                    ("working", planner.source_snapshot),
                    ("release", lambda: verification_manifest.source_commit(directory)),
                ):
                    with self.subTest(root=relative, consumer=consumer):
                        with self.assertRaises(error) as refused:
                            invoke()
                        if error is subprocess.CalledProcessError:
                            self.assertEqual(refused.exception.returncode, 128)
                        else:
                            self.assertIn("repository top", str(refused.exception))
        with mock.patch.dict(os.environ, PATH=str(self.root.parent / "outside")), self.assertRaises(OSError):
            source_snapshot(self.root)

    def test_shared_capture_refuses_actual_late_axes_and_preserves_clean_schema(self):
        # S1/S6/S9/S11: the clean legacy five fields retain empty Git-byte
        # digests. Cross late index/HEAD/untracked bytes during the shared read;
        # actual callback and exact capture refusal must both be observed.
        # S12 cause graph: formal versus working view selects no tree versus
        # one bracketed read-only tree, both with two raw reads. Stable inputs keep
        # original facts/hash/tuple semantics; any persistent late axis rejects.
        # Working additionally crosses a real index change at the tree-name read: its
        # returned tree cannot belong to older facts. No phase fence is cached.
        import rust_test_plan as planner
        clean = source_snapshot(self.root)
        self.assertEqual(set(clean), {"commit", "tree", "clean", "working_diff_digest", "status_digest"})
        self.assertEqual((clean["working_diff_digest"], clean["status_digest"]), (digest_bytes(b""),) * 2)
        original_reader = formal_sources._git_output
        with mock.patch.object(formal_sources, "_git_output", wraps=original_reader) as reads:
            default = formal_sources.source_facts(self.root)
        self.assertEqual(set(default), {"revision", "tree", "index_entries", "staged", "working", "patch", "untracked", "status"})
        self.assertEqual(sum(call.args[2:] == ("status", "--porcelain=v1", "--untracked-files=all") for call in reads.call_args_list), 2)
        self.assertFalse(any(call.args[2:] == ("write-tree",) for call in reads.call_args_list))
        with mock.patch.object(planner, "ROOT", self.root), \
                mock.patch.object(formal_sources, "_git_output", wraps=original_reader) as reads:
            digest, facts = planner.working_subject()
        self.assertEqual({key: value for key, value in facts.items() if key != "index_tree"}, default)
        self.assertEqual(facts["index_tree"], self.git("write-tree"))
        self.assertEqual(sum(call.args[2:] == ("status", "--porcelain=v1", "--untracked-files=all") for call in reads.call_args_list), 2)
        self.assertFalse(any(call.args[2:] == ("write-tree",) for call in reads.call_args_list))
        with mock.patch.object(planner, "ROOT", self.root):
            self.assertEqual(planner.source_snapshot(), (digest, facts["patch"]))
        for working, axis in ((working, axis) for working in (False, True)
                              for axis in ("index", "revision", "untracked", *(("tree-names",) if working else ()))):
            with self.subTest(working=working, axis=axis):
                hit = False
                if axis == "untracked":
                    (self.root / "fresh.rs").write_bytes(b"first bytes")
                if axis == "tree-names":
                    # An empty stage entry requires the extra intent-to-add reads.
                    (self.root / "empty.rs").write_bytes(b"")
                    self.git("add", "empty.rs")
                def read(root, environment, *arguments):
                    nonlocal hit
                    result = original_reader(root, environment, *arguments)
                    boundary = ("ls-tree", "-r", "--name-only", "-z", self.git("rev-parse", "HEAD").decode().strip()) if axis == "tree-names" else ("status", "--porcelain=v1", "--untracked-files=all")
                    if not hit and arguments == boundary:
                        hit = True
                        original = (self.root / "transitive.rs").read_bytes()
                        if axis in {"index", "revision", "tree-names"}:
                            (self.root / "transitive.rs").write_text(axis + "\n")
                            self.git("add", ".")
                        if axis in {"index", "tree-names"}:
                            (self.root / "transitive.rs").write_bytes(original)
                        if axis == "revision":
                            self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                                     "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "late revision")
                        if axis == "untracked":
                            (self.root / "fresh.rs").write_bytes(b"second bytes")
                    return result
                with mock.patch.object(planner, "ROOT", self.root), \
                        mock.patch.object(formal_sources, "_git_output", side_effect=read), \
                        self.assertRaisesRegex(ValueError, "Source changed while capturing Git source facts"):
                    planner.working_subject() if working else source_snapshot(self.root)
                self.assertTrue(hit)

    def test_readonly_index_tree_matches_git_for_dirty_paths_modes_and_intent(self):
        # I1 staged content + nested byte sorting + Unicode; I2 staged empty
        # versus unstaged intent-to-add; I3 executable/symlink/gitlink modes;
        # I4 a staged deletion. Every row compares with Git's actual tree.
        import formal_sources
        nested = self.root / "prefix"
        nested.mkdir()
        (nested / "Unicode 空格.rs").write_bytes(b"nested bytes\n")
        (self.root / "prefix.txt").write_bytes(b"prefix file\n")
        (self.root / "staged-empty.rs").write_bytes(b"")
        self.git("add", ".")
        (self.root / "intent.rs").write_bytes(b"not staged\n")
        self.git("add", "-N", "intent.rs")
        fresh = self.root / "H Unicode 路径.rs"
        fresh.write_bytes(b"untracked\n")
        self.git("update-index", "--skip-worktree", "prefix.txt")
        blob = self.git("rev-parse", ":transitive.rs").decode().strip()
        head = self.git("rev-parse", "HEAD").decode().strip()
        for mode, oid, name in (("100755", blob, "executable"), ("120000", blob, "symbolic-link"),
                                ("160000", head, "submodule")):
            self.git("update-index", "--add", "--cacheinfo", f"{mode},{oid},{name}")
            expected = self.git("write-tree")
            with mock.patch.object(formal_sources, "_git_output", wraps=formal_sources._git_output) as reads:
                captured = formal_sources.source_facts(self.root, with_index_tree=True)
            self.assertEqual(captured["index_tree"], expected, f"I1-I3 {mode}")
            self.assertEqual(captured["revision"], self.git("rev-parse", "HEAD"))
            self.assertEqual(captured["tree"], self.git("rev-parse", "HEAD^{tree}"))
            self.assertEqual(captured["index_entries"], self.git("ls-files", "--stage", "-z"))
            self.assertEqual(captured["untracked"], {fresh.name: fresh.read_bytes()})
            self.assertFalse(any(call.args[2:] == ("write-tree",) for call in reads.call_args_list))
        self.git("update-index", "--force-remove", "transitive.rs")
        self.assertEqual(formal_sources.source_facts(self.root, with_index_tree=True)["index_tree"],
                         self.git("write-tree"), "I4 deletion")

    def test_shared_capture_preserves_unlocked_stat_only_default_and_alternate_indices(self):
        # I8 a tracked file changes only its stat data with no index lock held.
        # Both the default and same-root alternate index retain every byte and
        # mtime; real content changes must still appear in both source frames.
        import formal_sources
        tracked = self.root / "transitive.rs"
        original = tracked.read_bytes()
        default = self.root / ".git/index"
        self.git("config", "diff.autoRefreshIndex", "true")
        for alternate in (False, True):
            with self.subTest(alternate=alternate):
                tracked.write_bytes(original)
                self.git("update-index", "--refresh")
                selected = default
                environment = dict(detached_git_environment(), GIT_OPTIONAL_LOCKS="0")
                if alternate:
                    selected = self.root.parent / "selected.index"
                    selected.write_bytes(default.read_bytes())
                    environment.update(GIT_DIR=str(self.root / ".git"),
                                       GIT_WORK_TREE=str(self.root), GIT_INDEX_FILE=str(selected))
                with mock.patch.dict(os.environ, environment, clear=True):
                    baseline = formal_sources.source_facts(self.root, with_index_tree=True)
                    protected = {p: (p.read_bytes(), p.stat().st_mtime_ns)
                                 for p in {default, selected}}
                    stamp = tracked.stat()
                    os.utime(tracked, ns=(stamp.st_atime_ns, stamp.st_mtime_ns + 10_000_000_000))
                    self.assertEqual(formal_sources.source_facts(self.root, with_index_tree=True), baseline)
                    for path, expected in protected.items():
                        self.assertEqual((path.read_bytes(), path.stat().st_mtime_ns), expected,
                                         "read-only capture refreshed the caller's unlocked index")
                    tracked.write_bytes(original + b"actual source change\n")
                    edited = formal_sources.source_facts(self.root, with_index_tree=True)
                    self.assertNotEqual(edited["working"], baseline["working"])
                    self.assertNotEqual(edited["patch"], baseline["patch"])
                    for path, expected in protected.items():
                        self.assertEqual((path.read_bytes(), path.stat().st_mtime_ns), expected)
                    self.assertFalse((self.root / ".git/index.lock").exists())

    def test_parallel_source_capture_never_takes_or_changes_the_callers_index_lock(self):
        # I5 lock occupied + concurrent readers -> the same exact index tree;
        # I6 neither index bytes nor the other owner's lock changes. This is an
        # actual filesystem/Git conflict, not a mocked lock-success response.
        from concurrent.futures import ThreadPoolExecutor
        import formal_sources
        expected = self.git("write-tree")
        index = self.root / ".git/index"
        index_bytes = index.read_bytes()
        index_stat = index.stat().st_mtime_ns
        lock = self.root / ".git/index.lock"
        lock_bytes = b"owned by another source-capture process\n"
        lock.write_bytes(lock_bytes)
        with ThreadPoolExecutor(max_workers=4) as executor:
            captured = list(executor.map(lambda _: formal_sources.source_facts(self.root, with_index_tree=True), range(12)))
        self.assertTrue(all(facts["index_tree"] == expected for facts in captured))
        self.assertEqual(index.read_bytes(), index_bytes)
        self.assertEqual(index.stat().st_mtime_ns, index_stat)
        self.assertEqual(lock.read_bytes(), lock_bytes)

    def test_readonly_index_tree_supports_the_git_sha256_object_format(self):
        # I7 both supported Git object formats retain their actual identity;
        # no SHA1-only truncation or repository object write is introduced.
        import formal_sources
        root = self.root.parent / "sha256-source"
        prepare_source_fixture(root, object_format="sha256")
        expected = subprocess.check_output(["git", "-C", str(root), "write-tree"], env=detached_git_environment())
        self.assertEqual(len(expected.strip()), 64)
        self.assertEqual(formal_sources.source_facts(root, with_index_tree=True)["index_tree"], expected)

    def test_git_normalized_windows_checkout_preserves_source_identity(self):
        # E1 a clean LF checkout completes; E2 Windows-style autocrlf checkout
        # changes physical bytes but not Git source. The same receipt validates.
        # E3 a semantic edit after conversion still rejects, so normalization
        # cannot conceal a changed runner or weaken the clean-source boundary.
        runner = self.root / "scripts/ci/check_formal.sh"
        self.git("config", "core.autocrlf", "false")
        self.assertFalse(b"\r\n" in runner.read_bytes(), "LF fixture")
        passed = finish_record(begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110)
        windows = self.root.parent / "windows-checkout"
        subprocess.run(["git", "clone", "--quiet", "--no-hardlinks", "--config", "core.autocrlf=true",
                        str(self.root), str(windows)], check=True, capture_output=True,
                       env=detached_git_environment())
        runner = windows / "scripts/ci/check_formal.sh"
        self.assertTrue(b"\r\n" in runner.read_bytes(), "CRLF fixture")
        validate_record(passed, windows, 110)
        runner.write_bytes(runner.read_bytes() + b"exit 23\r\n")
        with self.assertRaisesRegex(ValueError, "clean release source"):
            validate_record(passed, windows, 110)


if __name__ == "__main__":
    strict_unittest.main()
