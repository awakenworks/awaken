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

import formal_budget
import formal_plan
from formal_run_evidence import FULL_SELECTION, ROOT, begin_record, finish_record, validate_record, write_record, begin_artifacts, finish_artifacts, verify_artifacts


def nested_shell_function(source: str, name: str) -> str:
    marker = f"  {name}() {{\n"
    body = source.split(marker, 1)[1].split("\n  }", 1)[0]
    return f"{name}() {{\n{body}\n}}"


def formal_gate_initialization(root: Path) -> str:
    source = (root / "scripts/ci/check_formal.sh").read_text()
    return source.split("\npython3 scripts/ci/check_feature_coverage.py", 1)[0]


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


def prepare_source_fixture(root: Path, extra_sources: dict[str, str] | None = None) -> None:
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
    for arguments in (("init", "--quiet"), ("config", "core.autocrlf", "false"), ("add", "."),
                      ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                       "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "fixture")):
        subprocess.run(["git", *arguments], cwd=root, check=True, capture_output=True)


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
        (bindir / tool).symlink_to(resolved)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="awaken-proof-receipt-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "repo"
        prepare_source_fixture(self.root)

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, stderr=subprocess.STDOUT)

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
            f"source {shlex.quote(str(self.root / 'scripts/ci/_cargo_target.sh'))}\n"
            f"CARGO_TARGET_DIR={shlex.quote(str(target))}\n"
            "kani_jobs=1\n"
            "kani_batch_timeout_seconds=37\n"
            "formal_failure_kind= formal_failure_stage= formal_failure_subject= "
            "formal_failure_deadline_seconds=\n"
            "run_with_deadline() { printf '%s\\n' \"$1\" > \"$DEADLINE_CALL\"; shift; \"$@\"; }\n"
            + function
            + "\nrun_kani awaken-dispatch --no-default-features --harness exact_claim\n"
        )
        result = subprocess.run(
            ["/bin/bash", "-c", script],
            env=dict(os.environ, PATH=str(bindir), CALLS=str(calls), DEADLINE_CALL=str(deadline_call)),
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
        self.assertEqual(deadline_call.read_text(), "37\n")

    def test_kani_batch_outcomes_preserve_the_exact_first_failure(self):
        # Causes: C1 wrapper success, C2 ordinary Cargo failure, C3 deadline
        # status 124. Effects: E1 no failure metadata; E2 exact 23 plus
        # execution-failed; E3 exact 124 plus resource-bound/package/deadline.
        # Constraints: 124 is the shared deadline contract and no later receipt
        # step may replace it. Decision rules: R1=C1->E1, R2=C2->E2, R3=C3->E3.
        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        function = nested_shell_function(source, "run_kani")
        for wrapped_status, expected in (
            (0, (0, "", "", "", "")),
            (23, (23, "execution-failed", "cargo-kani", "awaken-provider", "")),
            (124, (124, "resource-bound", "cargo-kani", "awaken-provider", "37")),
        ):
            with self.subTest(wrapped_status=wrapped_status):
                script = (
                    "set -uo pipefail\nCARGO_TARGET_DIR=/build/fixture\nkani_jobs=1\n"
                    "kani_batch_timeout_seconds=37\n"
                    "formal_failure_kind= formal_failure_stage= formal_failure_subject= "
                    "formal_failure_deadline_seconds=\n"
                    f"run_with_deadline() {{ return {wrapped_status}; }}\n"
                    + function
                    + "\nrun_kani awaken-provider --harness exact\n"
                    "status=$?\nprintf '%s\\n' \"$status\" \"$formal_failure_kind\" "
                    "\"$formal_failure_stage\" \"$formal_failure_subject\" "
                    "\"$formal_failure_deadline_seconds\"\n"
                )
                result = subprocess.run(["/bin/bash", "-c", script], capture_output=True,
                                        text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(tuple([int(result.stdout.splitlines()[0]),
                                        *result.stdout.splitlines()[1:]]), expected)

    def test_pinned_solver_selection_and_strict_missing_tool_policy(self):
        # Z1 installed exact version + proof success => invoke the exact harness.
        # Z2 wrong/unreadable version => fail before proof. Z3 missing solver =>
        # mark missing, which fails strict mode and is explicit in diagnostic mode.
        # Z4 proof failure => propagate. Execute the real runner selection and its
        # final strict check with a solver process stub; this is not proof evidence.
        source = (self.root / "scripts/ci/check_formal.sh").read_text()
        # The real runner decides the solver for every batch, so execute it
        # (and its pinned-version helper) on the planned batch of the one
        # harness whose own tag selects Z3 alone.
        import formal_plan
        runner = source[source.index("  run_kani() {"):source.index("  # The batches are derived, not listed")]
        [batch] = [batch for batch in formal_plan.batches(ROOT, "full")
                   if batch.selection.profile == "solver-z3-v1"]
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
                script += "kani_jobs=1\nkani_batch_timeout_seconds=1\nawaken_configure_kani_driver_target() { :; }\n"
                script += "run_with_deadline() { shift; printf '%s\\n' \"$*\" > \"$CALLS\"; return \"$PROOF_EXIT\"; }\n"
                result = subprocess.run(["/bin/bash", "-c", script + selection + strict_check],
                                        env=dict(os.environ, PATH=str(bindir), CALLS=str(calls), PROOF_EXIT=str(proof_exit)),
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, expected, result.stderr)
                invoked = installed and reported == version and version_exit == 0
                self.assertEqual(calls.exists(), invoked)
                if invoked:
                    self.assertEqual(calls.read_text().strip(), "cargo kani -p awaken-protocol-managed --output-format terse -j 1 --solver z3 --exact --harness "
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
                if installed:
                    (bindir / solver).write_text("#!/bin/sh\nexit 0\n")
                    (bindir / solver).chmod(0o755)
                calls = bindir / "calls"
                script = (f"set -euo pipefail\nmissing=0\nrequire_tools={strict}\nz3_version=unused\n"
                          "kani_solver_tools=/released/kani/bin\nkani_jobs=1\nkani_batch_timeout_seconds=1\n"
                          "awaken_configure_kani_driver_target() { :; }\n"
                          "run_with_deadline() { shift; printf '%s\\n' \"$*\" > \"$CALLS\"; }\n")
                invocation = f"  run_kani awaken-protocol-managed --harness a --solver {solver} --harness b\n"
                result = subprocess.run(["/bin/bash", "-c", script + runner + invocation + strict_check],
                                        env=dict(os.environ, PATH=str(bindir) + os.pathsep + "/usr/bin:/bin", CALLS=str(calls)),
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
                        "  scripts/ci/formal_run_evidence.py|scripts/ci/formal_budget.py) exec " + shlex.quote(sys.executable) + " \"$@\" ;;\n"
                        "  *) touch \"$CHECKER_STARTED\"; exit 23 ;;\nesac\n")
        shim.chmod(0o755)
        env = dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                   AWAKEN_FORMAL_RUN_EVIDENCE=str(path), CHECKER_STARTED=str(checker_started))
        patch = self.root / "scripts/ci/patches/cbmc-smt-type-dependencies.patch"
        original = patch.read_bytes()
        for stage, expected in (("checker", 23), ("tool-selection", 1)):
            with self.subTest(stage=stage):
                shutil.rmtree(self.root / "target", ignore_errors=True)
                checker_started.unlink(missing_ok=True)
                if stage == "tool-selection":
                    patch.write_bytes(original + b"\n")
                write_record(path, finish_record(begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110))
                result = subprocess.run(["bash", "scripts/ci/check_formal.sh", "--require-tools"],
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
            + shlex.quote(sys.executable) + " \"$@\" ;;\n"
            "  *) touch \"$CHECKER_STARTED\"; exit 0 ;;\n"
            "esac\n"
        )
        shim.chmod(0o755)
        path = self.root / "target/test-timings/formal-run.json"
        for override in ("0", str(formal_budget.KANI_BATCH_TIMEOUT_CEILING_SECONDS + 1)):
            with self.subTest(override=override):
                shutil.rmtree(self.root / "target", ignore_errors=True)
                checker_started.unlink(missing_ok=True)
                result = subprocess.run(
                    ["bash", "scripts/ci/check_formal.sh", "--require-tools"],
                    cwd=self.root,
                    env=dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                             AWAKEN_FORMAL_RUN_EVIDENCE=str(path),
                             AWAKEN_KANI_BATCH_TIMEOUT_SECONDS=override,
                             CHECKER_STARTED=str(checker_started)),
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
            + shlex.quote(sys.executable) + ' "$@"\n'
        )
        shim.chmod(0o755)
        path = self.root / "target/test-timings/formal-run.json"
        body = initialization + (
            "\nformal_failure_kind=resource-bound\n"
            "formal_failure_stage=cargo-kani\n"
            "formal_failure_subject=awaken-provider-genai\n"
            "formal_failure_deadline_seconds=37\nexit 124\n"
        )
        result = subprocess.run(
            ["bash", "-c", body, "scripts/ci/check_formal.sh", "--require-tools"],
            cwd=self.root,
            env=dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                     AWAKEN_FORMAL_RUN_EVIDENCE=str(path)),
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
        initialization = formal_gate_initialization(self.root)
        bindir = Path(self.temporary.name) / "cleanup-bin"
        bindir.mkdir()
        cleanup = bindir / "rm"
        cleanup.write_text('#!/bin/sh\necho cleanup-failure >&2\nexit 41\n')
        cleanup.chmod(0o755)
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
                result = subprocess.run(
                    ["bash", "-c", initialization + "\n" + metadata
                     + f"exit {body_status}\n",
                     "scripts/ci/check_formal.sh", "--require-tools"],
                    cwd=self.root,
                    env=dict(os.environ, PATH=str(bindir) + os.pathsep + os.environ["PATH"],
                             AWAKEN_FORMAL_RUN_EVIDENCE=str(path)),
                    capture_output=True, text=True, timeout=30,
                )
                self.assertEqual(result.returncode, body_status, result.stderr)
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
        initialization = formal_gate_initialization(self.root)
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
                result = subprocess.run(["bash", "-c", initialization + "\nformal_body_complete=1\nexit 0\n",
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
                result = subprocess.run(
                    ["bash", "-c", initialization + "\n" + body + "\n",
                     "scripts/ci/check_formal.sh", "--require-tools"],
                    cwd=self.root,
                    env=dict(os.environ, AWAKEN_FORMAL_RUN_EVIDENCE=str(path)),
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

    def test_external_group_termination_cannot_certify_partial_formal_run(self):
        # C1 the receipt and EXIT owner are installed; C2 an external TERM
        # reaches the gate and its active child as one process group before the
        # last proof. E1 the process exits 143, E2 the exact clean source gets
        # Not-passed, E3 its diagnostic bundle seals Failed. R1=C1+C2->E1-E3.
        # This is the real interruption shape that once left a strict Passed
        # JSON after only manifest self-tests. The child starts before `ready`
        # so the group signal reaches both owners; no proof is simulated Passed.
        initialization = formal_gate_initialization(self.root)
        path = self.root / "target/test-timings/formal-run.json"
        process = subprocess.Popen(
            ["bash", "-c", initialization + "\nsleep 30 & child=$!\nprintf 'ready\\n'\nwait \"$child\"\n",
             "scripts/ci/check_formal.sh", "--require-tools"],
            cwd=self.root,
            env=dict(os.environ, AWAKEN_FORMAL_RUN_EVIDENCE=str(path)),
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
            _, stderr = process.communicate(timeout=10)
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
        record = finish_record(begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110)
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
        passed = finish_record(begin_record(self.root, True, 100, FULL_SELECTION), self.root, False, 110)
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
