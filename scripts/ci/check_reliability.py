#!/usr/bin/env python3
"""Validate the one reliability evidence path and operational objectives."""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock
from _native_shell import bash_executable


ROOT = Path(__file__).resolve().parents[2]
ASSUMPTIONS = ROOT / "formal" / "assumptions.json"
RELIABILITY_SCRIPT = ROOT / "scripts" / "ci" / "check-reliability.sh"
GENERATED_WORK_DIRS = {".codex-work", ".pnpm-store", "outputs", "reports"}

REQUIRED_SURFACES = (
    "crates/devtools/awaken-reliability-testkit/src/lib.rs",
    "crates/server/awaken-dispatch-testkit/src/conformance/reliability.rs",
    "crates/stores/awaken-store-fs/tests/recovery.rs",
    "crates/stores/awaken-store-sqlite/tests/failure_atomicity.rs",
    "crates/stores/awaken-session-store/tests/process_crash_outbox.rs",
    "crates/stores/awaken-credential-store/tests/process_crash_creation.rs",
    "crates/control/awaken-config-store/tests/process_crash_audit.rs",
    "e2e/managed_session_config_restart_e2e.mjs",
    "e2e/durable_soak_fairness_e2e.mjs",
    "scripts/ci/postgres_recovery_drill.sh",
    "fuzz/fuzz_targets/sse_chunks.rs",
    "fuzz/fuzz_targets/thread_commit_wire.rs",
    "fuzz/fuzz_targets/tool_batch_transitions.rs",
)

REQUIRED_TOOL_GATES = (
    "cargo +nightly fuzz run sse_chunks",
    "cargo +nightly fuzz run thread_commit_wire",
    "cargo +nightly fuzz run tool_batch_transitions",
    "--re 'permits_call_transition|next_execution_attempt'",
    "cargo clean -p awaken-runtime-contract",
    "cargo clean -p awaken-tool-contract",
    "cargo +nightly miri test -p awaken-mcp-wire --lib",
    "-Zsanitizer=address",
    "-Zsanitizer=thread",
    "cargo +nightly test -p awaken-worker --lib",
    "credential_liveness::tests::heartbeat_snapshot_never_mixes_concurrent_publication_batches",
    'python3 scripts/ci/check_reliability.py --tsan-log "$tsan_log"',
    "cargo mutants",
    "cleanup_mutation_artifacts",
    'export CARGO_TARGET_DIR="${CARGO_TARGET_DIR%/}/reliability-mutation-build"',
    'export CARGO_BUILD_BUILD_DIR="$CARGO_TARGET_DIR"',
    'python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/sse/mutants.out"',
    'python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/tool-batch/mutants.out"',
    'python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/session-event-observations/mutants.out"',
    'python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/session-realization-observations/mutants.out"',
    'python3 scripts/ci/check_reliability.py --mutants "$mutation_evidence/session-work-observations/mutants.out"',
)


def tsan_workload_passed(output: str) -> bool:
    """The single exact libtest target must execute, not merely compile."""
    summaries = re.findall(
        r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;",
        output,
        re.MULTILINE,
    )
    return summaries == [("ok", "1", "0", "0")]


def mutation_evidence_passed(directory: Path) -> bool:
    """Qualify cargo-mutants' own receipt; build failures are not test kills."""
    try:
        receipt = json.loads((directory / "outcomes.json").read_text())
        outcomes = receipt["outcomes"]
        total = receipt["total_mutants"]
        if type(total) is not int or total <= 0 or len(outcomes) != total + 1:
            return False
        planned = json.loads((directory / "mutants.json").read_text())
        planned_names = {mutant["name"] for mutant in planned}
        if len(planned) != total or len(planned_names) != total:
            return False
        if any(type(receipt[key]) is not int for key in ("caught", "missed", "timeout", "unviable", "success")):
            return False
        if receipt["caught"] != total or any(receipt[key] != 0 for key in ("missed", "timeout", "unviable", "success")):
            return False
        names = set()
        for index, outcome in enumerate(outcomes):
            baseline = index == 0
            if baseline:
                if outcome["scenario"] != "Baseline" or outcome["summary"] != "Success":
                    return False
            else:
                name = outcome["scenario"]["Mutant"]["name"]
                if not name or name in names or outcome["summary"] != "CaughtMutant":
                    return False
                names.add(name)
            phases = outcome["phase_results"]
            if [phase["phase"] for phase in phases] != ["Build", "Test"] or phases[0]["process_status"] != "Success":
                return False
            status = phases[1]["process_status"]
            if (status != "Success" if baseline else status != {"Failure": 101}):
                return False
            log = (directory / outcome["log_path"]).read_text()
            summaries = re.findall(r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;", log, re.MULTILINE)
            if baseline:
                if not summaries or not sum(int(row[1]) for row in summaries) or any(row[0] != "ok" or row[2] != "0" or row[3] != "0" for row in summaries):
                    return False
            elif not any(row[0] == "FAILED" and int(row[2]) > 0 for row in summaries):
                return False
        return names == planned_names
    except (OSError, ValueError, KeyError, TypeError, IndexError):
        return False


def validation_errors(
    assumptions: dict[str, object],
    script: str,
    existing: set[str],
    crash_sources: dict[str, str],
) -> list[str]:
    errors: list[str] = []
    missing = sorted(set(REQUIRED_SURFACES) - existing)
    if missing:
        errors.append("missing reliability surfaces: " + ", ".join(missing))
    for marker in REQUIRED_TOOL_GATES:
        if marker not in script:
            errors.append(f"reliability gate missing {marker!r}")

    rows = assumptions.get("assumptions")
    if not isinstance(rows, list):
        return errors + ["formal assumptions are not a list"]
    for row in rows:
        if not isinstance(row, dict) or row.get("kind") == "semantic":
            continue
        objective = row.get("reliability_objective")
        if not isinstance(objective, dict):
            errors.append(f"{row.get('id')} has no reliability_objective")
            continue
        target = objective.get("target")
        if not isinstance(target, (int, float)) or not 0 < target <= 1:
            errors.append(f"{row.get('id')} has invalid objective target")
        for field in ("indicator", "window_days", "max_burn_rate", "scope"):
            if not objective.get(field):
                errors.append(f"{row.get('id')} objective missing {field}")
        for field in ("recovery_time_seconds", "recovery_point_seconds"):
            value = objective.get(field)
            if not isinstance(value, int) or value < 0:
                errors.append(f"{row.get('id')} objective has invalid {field}")
        drills = objective.get("drill_evidence")
        if not isinstance(drills, list) or not drills:
            errors.append(f"{row.get('id')} objective has no drill_evidence")
        else:
            for evidence in drills:
                if evidence not in existing:
                    errors.append(f"{row.get('id')} drill evidence missing: {evidence}")

    # The shared CrashProcess fixture is the sole child-kill orchestration
    # owner. Domain tests may define their child behavior, but must not copy the
    # parent-side polling/spawn implementation.
    for path, source in crash_sources.items():
        if "Command::new(std::env::current_exe" in source:
            errors.append(f"{path} duplicates the CrashProcess harness")
    return errors


def current_inputs() -> tuple[dict[str, object], str, set[str], dict[str, str]]:
    existing: set[str] = set()
    for directory, child_dirs, files in os.walk(ROOT):
        child_dirs[:] = [
            name
            for name in child_dirs
            if name not in {"target", ".git", *GENERATED_WORK_DIRS}
            and not name.startswith("mutants.out")
        ]
        parent = Path(directory)
        existing.update((parent / name).relative_to(ROOT).as_posix() for name in files)
    crash_sources = {
        relative: (ROOT / relative).read_text(encoding="utf-8")
        for relative in existing
        if Path(relative).name.startswith("process_crash")
        or Path(relative).name == "failure_atomicity.rs"
    }
    return (
        json.loads(ASSUMPTIONS.read_text(encoding="utf-8")),
        RELIABILITY_SCRIPT.read_text(encoding="utf-8"),
        existing,
        crash_sources,
    )


# Stub cargo variants for the tool-query decision table: every other
# subcommand must still succeed, so the gate reaches the query it is about.
QUERY_FAILS_CARGO = """#!/usr/bin/env bash
if [ "$1" = awaken-ci-deadline ]; then entry=$(command -v "cargo-$1"); exec "$entry" "$@"; fi
case "$*" in
  "install --list") exit 3 ;;
  *--list*) echo 'reliability_fixture: test' ;;
  *--show-output*) echo 'test reliability_fixture ... ok'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored;' ;;
  *"heartbeat_snapshot_never_mixes_concurrent_publication_batches"*)
    echo 'test result: ok. 1 passed; 0 failed; 0 ignored;' ;;
esac
"""
PARTIAL_LIST_CARGO = """#!/usr/bin/env bash
if [ "$1" = awaken-ci-deadline ]; then entry=$(command -v "cargo-$1"); exec "$entry" "$@"; fi
case "$*" in
  "install --list") echo 'cargo-mutants 1' ;;
  *--list*) echo 'reliability_fixture: test' ;;
  *--show-output*) echo 'test reliability_fixture ... ok'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored;' ;;
  *"heartbeat_snapshot_never_mixes_concurrent_publication_batches"*)
    echo 'test result: ok. 1 passed; 0 failed; 0 ignored;' ;;
esac
"""


class ReliabilityCheckerTests(unittest.TestCase):
    @staticmethod
    def run_fixture(scripts: Path, environment: dict) -> subprocess.CompletedProcess:
        # Windows executes the real native admission/deadline and list/run
        # subprocesses. Bound the complete fixture with the existing process
        # owner so a test timeout cannot leave Bash descendants holding pipes.
        budget = 180 if os.name == "nt" else 20
        return subprocess.run(
            [sys.executable, str(scripts / "deadline.py"), str(budget), "--",
             bash_executable(), str(scripts / "check-reliability.sh"), "--require-tools"],
            env=environment, capture_output=True, text=True, encoding="utf-8", timeout=budget + 15,
        )

    @staticmethod
    def fixture_environment(binary: Path, **values) -> dict:
        # Git Bash initializes utility PATH before running the actual runner.
        # Keep the owned adapters first for every nested shell as well.
        setup = binary.parent / "fixture-environment.sh"
        setup.write_text('fixture_bin=$(cd -- "$RELIABILITY_FIXTURE_BIN" && pwd -P)\n'
                         'export PATH="$fixture_bin:$PATH"\n', encoding="utf-8", newline="\n")
        environment = dict(os.environ, PATH=f"{binary}{os.pathsep}{os.environ['PATH']}",
                           RELIABILITY_FIXTURE_BIN=binary.as_posix(), BASH_ENV=setup.as_posix(), **values)
        if os.name == "nt":
            from _native_test_launcher import native_fixture_launcher

            shutil.copyfile(native_fixture_launcher(), binary / "cargo.exe")
            environment.update(AWAKEN_FIXTURE_PROGRAM=bash_executable(),
                               AWAKEN_FIXTURE_SCRIPT=str(binary / "cargo"))
        return environment

    @staticmethod
    def runner_fixture(root: Path) -> Path:
        # D0/Q0: actual required execution imports its transitive closure and
        # reads a real private index, before reaching the tool decision. Reuse
        # the same fixture owner; no stale copy list or fake Git source proof.
        from check_rust_test_execution import ExecutionTests
        return ExecutionTests.runner_fixture(root, "scripts/ci/check-reliability.sh",
                                              "scripts/ci/_cargo_target.sh", "scripts/ci/_provider_environment.sh")

    @staticmethod
    def python_adapter() -> str:
        # Only domain/tool validation is stubbed in these tool-orchestration
        # tests. Source capture, required list/run and deadline must execute.
        return f'''#!/usr/bin/env bash
case "$1" in
  *deadline.py|*cargo-awaken-ci-deadline|*check_rust_test_execution.py) exec {shlex.quote(Path(sys.executable).as_posix())} "$@" ;;
  *) exit 0 ;;
esac
'''

    def test_mutant_receipts_require_executed_tests_and_complete_coverage(self) -> None:
        # M1 complete nonempty batch + passing baseline + assertion failure for
        # every mutant => accept. M2 baseline absent/failed/zero/ignored => reject.
        # M3 missed/build-failed/timed-out/duplicate/incomplete batch => reject.
        # These counterexamples change independent receipt AND execution facts;
        # a reassuring summary cannot replace a real test result in the tool log.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            baseline_log = "test result: ok. 2 passed; 0 failed; 0 ignored;"
            mutant_log = "test result: FAILED. 1 passed; 1 failed; 0 ignored;"
            baseline = {"scenario": "Baseline", "summary": "Success", "log_path": "baseline.log", "phase_results": [
                {"phase": "Build", "process_status": "Success"}, {"phase": "Test", "process_status": "Success"}]}
            mutant = {"scenario": {"Mutant": {"name": "one"}}, "summary": "CaughtMutant", "log_path": "mutant.log", "phase_results": [
                {"phase": "Build", "process_status": "Success"}, {"phase": "Test", "process_status": {"Failure": 101}}]}
            receipt = {"outcomes": [baseline, mutant], "total_mutants": 1, "caught": 1, "missed": 0, "timeout": 0, "unviable": 0, "success": 0}
            (directory / "mutants.json").write_text(json.dumps([{"name": "one"}]))

            def check(value: dict, base: str = baseline_log, killed: str = mutant_log) -> bool:
                (directory / "outcomes.json").write_text(json.dumps(value))
                (directory / "baseline.log").write_text(base)
                (directory / "mutant.log").write_text(killed)
                return mutation_evidence_passed(directory)

            self.assertTrue(check(receipt), "M1")
            for base in ("", baseline_log.replace("2 passed", "0 passed"), baseline_log.replace("0 ignored", "1 ignored"), mutant_log):
                self.assertFalse(check(receipt, base=base), "M2")
            for killed in ("", baseline_log, "process timed out"):
                self.assertFalse(check(receipt, killed=killed), "M3 log")
            for field in ("missed", "timeout", "unviable", "success", "total_mutants", "caught"):
                changed = json.loads(json.dumps(receipt))
                changed[field] += 1
                self.assertFalse(check(changed), f"M3 {field}")
            for index, phase in ((0, 1), (1, 0), (1, 1)):
                changed = json.loads(json.dumps(receipt))
                changed["outcomes"][index]["phase_results"][phase]["process_status"] = "Timeout"
                self.assertFalse(check(changed), "M2/M3 phase")
            self.assertFalse(check({**receipt, "outcomes": [mutant]}), "M2 absent baseline")
            self.assertFalse(check({**receipt, "total_mutants": 0, "caught": 0, "outcomes": [baseline]}), "M3 empty")
            self.assertFalse(check({**receipt, "total_mutants": 2, "caught": 2, "outcomes": [baseline, mutant, mutant]}), "M3 duplicate")
            self.assertFalse(check({**receipt, "caught": True}), "M3 malformed count")
            (directory / "mutants.json").write_text(json.dumps([{"name": "other"}]))
            self.assertFalse(check(receipt), "M3 result for an unselected mutant")
            (directory / "mutants.json").write_text(json.dumps([{"name": "one"}, {"name": "omitted"}]))
            self.assertFalse(check(receipt), "M3 missing result cannot shrink planned coverage")

    def test_runner_output_directories(self) -> None:
        for case in ("fresh", "existing", "nested"):
            self._assert_runner_case(case)

    def test_runner_blocked_output_is_preserved(self) -> None:
        self._assert_runner_case("blocked")

    def test_runner_tool_failures_stop_and_cleanup(self) -> None:
        for case in ("tool-failure", "missing-init", "fuzz-lock-stale"):
            self._assert_runner_case(case)

    def _assert_runner_case(self, case: str) -> None:
        # Directory/tool graph: D1 output parent missing/existing/nested; D2 a
        # regular file blocks creation; D3 a mutation command fails. Effects:
        # D1 => initialize + run all 6 tools; D2 => zero mutation calls + failure;
        # D3 => stop + failure + cleanup. The real script runs in a disposable
        # checkout with process stubs: these prove orchestration, not Rust tests.
        # D4 a named mutation filter must select a function declared in the
        # configured implementation file. A still-existing facade that only
        # imports/re-exports the function is rejected by this structural oracle;
        # actual nonempty mutation generation and test kills remain cargo-mutants'
        # and the receipt checker’s responsibilities. Derive paths and selectors
        # from the real runner argv, without a second target registry.
        # D5 the shared UTC owner moved packages: its mutation must still run
        # the original Session Work consumer tests as well as the owner's tests.
        # D6 stale fuzz lock => exact preflight failure, zero fuzz/mutation calls;
        # a current lock passes the existing Cargo --locked reader before fuzz.
        # D7 each actual mutation package must be cleaned before and after the
        # batch. D8 cleanup/mutations use the isolated target AND build directory;
        # ordinary tests retain their original cache even after tool failure.
        # Removing the mkdir line must kill the fresh-directory positive control.
        with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scripts = self.runner_fixture(root)
            (root / "fuzz").mkdir()
            if case == "missing-init":
                runner = scripts / "check-reliability.sh"
                runner.write_text(runner.read_text().replace('mkdir -p "$mutation_evidence"\n', ""), encoding="utf-8", newline="\n")
            binary = root / "bin"
            binary.mkdir()
            cargo = '''#!/usr/bin/env bash
if [ "$1" = awaken-ci-deadline ]; then entry=$(command -v "cargo-$1"); exec "$entry" "$@"; fi
printf '%q ' "$@" >> "$CALL_LOG"
printf '\\n' >> "$CALL_LOG"
printf '%s|%s|%s\\n' "$CARGO_TARGET_DIR" "${CARGO_BUILD_BUILD_DIR:-}" "$1" >> "$CALL_TARGET_LOG"
case "$*" in
  "install --list") echo 'cargo-fuzz 1'; echo 'cargo-mutants 1' ;;
  *--list*) echo 'reliability_fixture: test' ;;
  *--show-output*) echo 'test reliability_fixture ... ok'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored;' ;;
  "+nightly metadata --locked --format-version 1")
    [ "$FAIL_FUZZ_LOCK" != 1 ] || exit 41 ;;
  "mutants "*)
    while [ "$1" != '--output' ]; do shift; done
    [ -d "$(dirname "$2")" ] || exit 9
    [ "$FAIL_MUTANT" != 1 ] || exit 7 ;;
  *"heartbeat_snapshot_never_mixes_concurrent_publication_batches"*)
    echo 'test result: ok. 1 passed; 0 failed; 0 ignored;' ;;
esac
'''
            for name, body in {"cargo": cargo, "python3": self.python_adapter(), "rustup": "#!/bin/sh\necho nightly-test\n", "rustc": "#!/bin/sh\necho 'host: x86_64-unknown-linux-gnu'\n"}.items():
                tool = binary / name
                tool.write_text(body, encoding="utf-8", newline="\n")
                tool.chmod(0o755)
            output = root / ("nested/output/evidence" if case == "nested" else "evidence")
            if case == "existing":
                output.mkdir()
            if case == "blocked":
                output.write_text("owned file must remain unchanged")
            env = self.fixture_environment(binary, CARGO_TARGET_DIR=(root / "target").as_posix(), CARGO_BUILD_BUILD_DIR=(root / "normal-build").as_posix(), AWAKEN_RELIABILITY_EVIDENCE_DIR=output.as_posix(), CALL_LOG=(root / "target/calls").as_posix(), CALL_TARGET_LOG=(root / "target/targets").as_posix(), FAIL_MUTANT=str(int(case == "tool-failure")), FAIL_FUZZ_LOCK=str(int(case == "fuzz-lock-stale")))
            result = self.run_fixture(scripts, env)
            calls = [line.rstrip() for line in (root / "target/calls").read_text().splitlines()]
            mutations = [line for line in calls if line.startswith("mutants ")]
            targets = [line.split("|") for line in (root / "target/targets").read_text().splitlines()]
            for target, build, command in targets:
                isolated = command in ("clean", "mutants")
                expected_target = root / "target" / "reliability-mutation-build" if isolated else root / "target"
                expected_build = expected_target if isolated else root / "normal-build"
                self.assertEqual(Path(target), expected_target, "D8 target isolation")
                self.assertEqual(Path(build), expected_build, "D8 build isolation")
            passed = case in ("fresh", "existing", "nested")
            self.assertEqual(result.returncode == 0, passed, result.stderr)
            self.assertEqual(len(mutations), 6 if passed else 0 if case in ("blocked", "fuzz-lock-stale") else 1)
            if case == "fuzz-lock-stale":
                self.assertEqual(result.returncode, 41, "D6 exact preflight refusal")
                self.assertEqual(calls[-1], "+nightly metadata --locked --format-version 1")
                self.assertFalse(any(" fuzz run " in call for call in calls), "D6 zero fuzz builds")
            else:
                self.assertEqual(calls[-1], "clean -p awaken-session-executor", "D2/D3 cleanup")
            if passed:
                preflight = calls.index("+nightly metadata --locked --format-version 1")
                self.assertLess(preflight, next(index for index, call in enumerate(calls) if " fuzz run " in call), "D6 locked preflight precedes fuzz")
                owners = set()
                for mutation in mutations:
                    argv = shlex.split(mutation)
                    owners.add(argv[argv.index("--package") + 1])
                    selected_source = ROOT / argv[argv.index("--file") + 1]
                    self.assertTrue(selected_source.is_file(), "D4 source exists")
                    if "--re" not in argv:
                        continue
                    selector = argv[argv.index("--re") + 1]
                    # Operator filters match generated mutation descriptions,
                    # not function names; the actual batch owns that check.
                    if selector.startswith("replace "):
                        continue
                    declared_names = re.findall(
                        r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+|async\s+)?fn\s+(\w+)",
                        selected_source.read_text(),
                    )
                    self.assertTrue(
                        any(re.search(selector, name) for name in declared_names),
                        f"D4 named filter must reach its implementation: {selected_source}",
                    )
                    if selector == "bounded_rfc3339_nanos":
                        test_packages = {
                            argv[index + 1]
                            for index, argument in enumerate(argv)
                            if argument == "--test-package"
                        }
                        self.assertEqual(
                            test_packages,
                            {argv[argv.index("--package") + 1], "awaken-worker-contract"},
                            "D5 shared UTC owner and original Work consumer both execute",
                        )
                def cleaned_packages(selected_calls: list[str]) -> set[str]:
                    packages = set()
                    for call in selected_calls:
                        argv = shlex.split(call)
                        if argv[0] == "clean":
                            packages.update(argv[index + 1] for index, arg in enumerate(argv) if arg == "-p")
                    return packages
                self.assertLessEqual(owners, cleaned_packages(calls[:calls.index(mutations[0])]), "D7 all mutation owners cleaned before")
                self.assertLessEqual(owners, cleaned_packages(calls[calls.index(mutations[-1]) + 1:]), "D7 all mutation owners cleaned after")
            if case == "blocked":
                self.assertEqual(output.read_text(), "owned file must remain unchanged")

    def test_a_failed_tool_query_is_not_reported_as_a_missing_tool(self) -> None:
        """Q1 the listing command fails; Q2 it succeeds but omits the tool.

        Both used to read "cargo-fuzz is required": the listing was piped
        straight into grep, so a pipeline that could not run at all (a failed
        fork under load) accused an installed tool of being absent, and the
        gate's verdict depended on machine load rather than on the machine.
        """

        for case, listing, expected in (
            ("query-fails", QUERY_FAILS_CARGO, "could not list installed cargo tools"),
            ("tool-absent", PARTIAL_LIST_CARGO, "cargo-fuzz is required"),
        ):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                scripts = self.runner_fixture(root)
                (root / "fuzz").mkdir()
                binary = root / "bin"
                binary.mkdir()
                for name, body in {
                    "cargo": listing,
                    "python3": self.python_adapter(),
                    "rustup": "#!/bin/sh\necho nightly-test\n",
                    "rustc": "#!/bin/sh\necho 'host: x86_64-unknown-linux-gnu'\n",
                }.items():
                    tool = binary / name
                    tool.write_text(body, encoding="utf-8", newline="\n")
                    tool.chmod(0o755)
                env = self.fixture_environment(
                    binary,
                    CARGO_TARGET_DIR=(root / "target").as_posix(),
                    AWAKEN_RELIABILITY_EVIDENCE_DIR=(root / "evidence").as_posix(),
                    CALL_LOG=(root / "calls").as_posix(),
                )
                result = self.run_fixture(scripts, env)
                self.assertNotEqual(result.returncode, 0, case)
                self.assertIn(expected, result.stderr, case)
                if case == "query-fails":
                    self.assertNotIn("is required by the release reliability gate", result.stderr,
                                     "Q1 a failed query must never accuse a tool")

    def test_self_test_groups_are_complete_disjoint_and_cli_selected(self) -> None:
        def identities(suite: unittest.TestSuite) -> set[str]:
            return {test.id() for test in suite}

        groups = {name: identities(self_test_suite(name))
                  for name in ("preflight", "directories", "faults")}
        complete = identities(unittest.defaultTestLoader.loadTestsFromTestCase(ReliabilityCheckerTests))
        self.assertEqual(identities(self_test_suite("all")), complete)
        self.assertEqual(complete, set.union(*groups.values()))
        self.assertEqual(len(complete), sum(map(len, groups.values())))
        prefix = f"{__name__}.ReliabilityCheckerTests."
        self.assertEqual(groups["directories"], {prefix + "test_runner_output_directories"})
        self.assertEqual(groups["faults"], {prefix + "test_runner_tool_failures_stop_and_cleanup"})
        self.assertIn(prefix + "test_runner_blocked_output_is_preserved", groups["preflight"])
        self.assertIn(prefix + "test_a_failed_tool_query_is_not_reported_as_a_missing_tool", groups["preflight"])
        # Exercise the real argument parser without recursively running this test.
        with mock.patch.object(unittest, "TextTestRunner") as runner:
            runner.return_value.run.return_value.wasSuccessful.return_value = True
            for group, selected in {"all": complete, **groups}.items():
                self.assertEqual(main(["--self-test", group]), 0)
                self.assertEqual(identities(runner.return_value.run.call_args.args[0]), selected)
            self.assertEqual(main(["--self-test"]), 0)
            self.assertEqual(identities(runner.return_value.run.call_args.args[0]), complete)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as rejected:
            main(["--self-test", "unknown"])
        self.assertEqual(rejected.exception.code, 2)

    def test_tsan_requires_one_executed_passing_workload(self) -> None:
        # Cause/effect table: T1 exact target executes and passes => accept;
        # T2 renamed/filter-missed target executes zero => reject; T3 ignored,
        # failed, absent, extra or duplicated summaries => reject. Exit status
        # is independently enforced by the shell's pipefail before this check.
        accepted = "test result: ok. 1 passed; 0 failed; 0 ignored; 39 filtered out;"
        self.assertTrue(tsan_workload_passed(accepted), "T1")
        for rejected in (
            "", "test result: ok. 0 passed; 0 failed; 0 ignored; 40 filtered out;",
            "test result: ok. 0 passed; 0 failed; 1 ignored; 39 filtered out;",
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; 39 filtered out;",
            accepted.replace("1 passed", "2 passed"), accepted + "\n" + accepted,
        ):
            self.assertFalse(tsan_workload_passed(rejected), "T2/T3")

    def test_decision_table_rejects_each_parallel_or_incomplete_path(self) -> None:
        # Cause/effect graph: C1 every surface/tool gate exists; C2 every
        # non-semantic external assumption has a bounded objective and drill;
        # C3 crash tests delegate to CrashProcess; C4 mutation artifacts are
        # cleaned after success, failure, or interruption. Effects: E1 accept
        # all; E2 missing gate rejects; E3 missing objective rejects; E4
        # duplicate process harness rejects; E5 missing mutation cleanup
        # rejects. Rules R1=C1+C2+C3+C4; R2=!C1; R3=!C2; R4=!C3; R5=!C4.
        inputs = current_inputs()
        self.assertEqual(validation_errors(*inputs), [], "R1")

        for marker in REQUIRED_TOOL_GATES:
            with self.subTest(missing_gate=marker):
                script_mutant = inputs[1].replace(marker, "removed_gate")
                self.assertTrue(validation_errors(inputs[0], script_mutant, inputs[2], inputs[3]), "R2")

        assumptions_mutant = json.loads(json.dumps(inputs[0]))
        next(
            row for row in assumptions_mutant["assumptions"] if row["kind"] != "semantic"
        ).pop("reliability_objective")
        self.assertTrue(
            validation_errors(assumptions_mutant, inputs[1], inputs[2], inputs[3]), "R3"
        )

        crash_mutant = dict(inputs[3])
        crash_mutant["process_crash_mutant.rs"] = "Command::new(std::env::current_exe())"
        self.assertTrue(
            validation_errors(inputs[0], inputs[1], inputs[2], crash_mutant), "R4"
        )

        cleanup_mutant = inputs[1].replace(
            "cleanup_mutation_artifacts", "mutation_artifacts_are_not_cleaned"
        )
        self.assertTrue(
            validation_errors(inputs[0], cleanup_mutant, inputs[2], inputs[3]), "R5"
        )

        # R6 restoring the former zero-test helper target or removing the
        # executed-test receipt check must fail the static release surface gate.
        for old, new in (
            ("cargo +nightly test -p awaken-worker --lib", "cargo +nightly test -p awaken-reliability-testkit --lib"),
            ('--tsan-log "$tsan_log"', "--unused-log"),
        ):
            self.assertTrue(validation_errors(inputs[0], inputs[1].replace(old, new), inputs[2], inputs[3]), "R6")


def self_test_suite(group: str) -> unittest.TestSuite:
    names = set(unittest.defaultTestLoader.getTestCaseNames(ReliabilityCheckerTests))
    selected = {
        "directories": {"test_runner_output_directories"},
        "faults": {"test_runner_tool_failures_stop_and_cleanup"},
    }
    if not set.union(*selected.values()) <= names:
        raise ValueError("a declared reliability self-test group lost its test")
    # Partition the nine native fixtures into three calls per group. Retain
    # each fixture's 180 seconds and the worker's 600 seconds including setup.
    # Keep all unclassified checker tests, including future ones, in preflight.
    selected["preflight"] = names - set.union(*selected.values())
    selected["all"] = names
    if group not in selected:
        raise ValueError("unknown reliability self-test group: " + group)
    return unittest.TestSuite(ReliabilityCheckerTests(name) for name in sorted(selected[group]))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("--self-test", nargs="?", const="all",
                        choices=("all", "preflight", "directories", "faults"))
    parser.add_argument("--tsan-log", type=Path)
    parser.add_argument("--mutants", type=Path)
    args = parser.parse_args(argv)
    if args.mutants is not None:
        if not mutation_evidence_passed(args.mutants):
            print("ERROR: mutation evidence must contain a passing executed baseline and every mutant caught by tests", file=sys.stderr)
            return 1
        print("mutation evidence: complete nonempty batch caught by executed tests")
        return 0
    if args.tsan_log is not None:
        if not tsan_workload_passed(args.tsan_log.read_text(encoding="utf-8")):
            print("ERROR: TSan must execute exactly one passing, non-ignored production workload", file=sys.stderr)
            return 1
        print("TSan production workload: 1 executed / 1 passed")
        return 0
    if args.self_test:
        suite = self_test_suite(args.self_test)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    errors = validation_errors(*current_inputs())
    if errors:
        print("\n".join(f"ERROR: {error}" for error in errors), file=sys.stderr)
        return 1
    print("reliability evidence: objectives, drills, tools, and crash owner are complete")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
