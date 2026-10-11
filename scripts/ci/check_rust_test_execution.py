#!/usr/bin/env python3
"""Qualify a required libtest run against its independently listed selection."""
from __future__ import annotations

import argparse
from collections import Counter
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

LISTED = re.compile(r"^(\S+): test$", re.MULTILINE)
EXECUTED = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)(?:,.*)?$", re.MULTILINE)
SUMMARY = re.compile(r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;", re.MULTILINE)
SKIP = re.compile(r"^(?:SKIPPED\b|\[skip\])", re.MULTILINE | re.IGNORECASE)


def execution_errors(inventory: str, results: str, exit_code: int,
                     required_cases: list[str] | None = None) -> list[str]:
    planned = Counter(LISTED.findall(inventory))
    observed = EXECUTED.findall(results)
    actual = Counter(name for name, _ in observed)
    errors: list[str] = []
    if exit_code:
        errors.append(f"test command failed with status {exit_code}")
    if not planned:
        errors.append("required selection is empty")
    missing = sorted(set(required_cases or []) - planned.keys())
    if missing:
        errors.append("required backend cases were not selected: " + ", ".join(missing))
    if actual != planned:
        errors.append(f"selection differs: missing={dict(planned - actual)}, unexpected={dict(actual - planned)}")
    if any(status != "ok" for _, status in observed):
        errors.append("required tests failed or were ignored")
    summaries = SUMMARY.findall(results)
    if not summaries or any(status != "ok" or int(failed) or int(ignored) for status, _, failed, ignored in summaries):
        errors.append("passing, non-skipped execution summaries are required")
    if sum(int(passed) for _, passed, _, _ in summaries) != len(observed):
        errors.append("summary counts differ from named test results")
    if SKIP.search(results):
        errors.append("a selected test skipped its fixture or behavior")
    return errors


class ExecutionTests(unittest.TestCase):
    @staticmethod
    def runner_fixture(fixture: Path, *entrypoint_inputs: str) -> Path:
        from test_deadline import INPUT_GLOBS as deadline_inputs

        root = Path(__file__).resolve().parents[2]
        inputs = {*deadline_inputs, "scripts/ci/_strict_unittest.py",
                  "scripts/ci/_rust_test_execution.sh", "scripts/ci/check_rust_test_execution.py",
                  "scripts/ci/rust_test_plan.py", "scripts/ci/crate_test.py", "scripts/ci/nextest_execution.py",
                  *entrypoint_inputs}
        for relative in sorted(inputs):
            target = fixture / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(root / relative, target)
        return fixture / "scripts/ci"

    @unittest.skipIf(os.name == "nt", "the Bash orchestration runs in the Linux gate")
    def test_portability_entrypoint_requires_the_current_launch_adapter_selection(self) -> None:
        # P0 the temporary entrypoint includes the deadline owner's declared
        # inputs -> its native-shell/Job adapters load before selection. Reuse
        # that declaration rather than a second, stale deadline dependency list.
        # P1 the exact current owner lists and executes => continue to E2E.
        # P2 empty selection or P3 former owner => reject before E2E, even when
        # Cargo itself returns success. Exercise the actual entrypoint/helper.
        selected = "sandbox_source::tests::configured_working_directory_drives_process_and_acp_from_one_view"
        for case in ("passing", "empty", "former-owner"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                scripts = self.runner_fixture(fixture, "scripts/ci/check-trusted-host-portability.sh",
                                              "scripts/ci/_cargo_target.sh", "scripts/ci/_provider_environment.sh")
                entrypoint = scripts / "check-trusted-host-portability.sh"
                if case == "former-owner":
                    source = entrypoint.read_text(encoding="utf-8")
                    entrypoint.write_text(source.replace(
                        "run_required_rust_tests -p awaken-execution-composition --lib ",
                        "run_required_rust_tests -p awaken-session-executor --lib ", 1), encoding="utf-8")
                binary = fixture / "bin"
                binary.mkdir()
                commands = {
                    "cargo": '''#!/usr/bin/env bash
if [ "$1" = awaken-ci-deadline ]; then shift 3; exec "$@"; fi
printf '%s\\n' "$*" >> "$CALL_LOG"
selection=portable_fixture
case "$*" in
  *configured_working_directory_drives_process_and_acp_from_one_view*)
    selection="$SELECTED"
    [[ " $* " == *" -p awaken-execution-composition "* && "$CASE" != empty ]] || selection=''
    ;;
esac
case "$*" in
  *--list*) [ -z "$selection" ] || echo "$selection: test" ;;
  *--show-output*)
    if [ -n "$selection" ]; then
      echo "test $selection ... ok"
      echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
    else
      echo 'test result: ok. 0 passed; 0 failed; 0 ignored;'
    fi ;;
esac
exit 0
''',
                    "git": '#!/bin/sh\n[ "$1" != rev-parse ] || echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n',
                    "uname": "#!/bin/sh\necho MINGW64_NT\n",
                    "cygpath": '#!/bin/sh\n[ "$1" = -w ] || exit 2\necho /owned/git-bash\n',
                    "node": '''#!/bin/sh
if [ "$1" = --input-type=module ]; then echo /owned/hand; else echo E2E >> "$CALL_LOG"; fi
''',
                }
                for name, source in commands.items():
                    command = binary / name
                    command.write_text(source, encoding="utf-8")
                    command.chmod(0o755)
                environment = dict(os.environ, PATH=f"{binary}:{os.environ['PATH']}", CASE=case,
                                   SELECTED=selected, CALL_LOG=str(fixture / "calls"),
                                   CARGO_TARGET_DIR=str(fixture / "target"))
                result = subprocess.run(["bash", str(entrypoint)], cwd=fixture, env=environment,
                                        capture_output=True, text=True, timeout=90)
                self.assertEqual(result.returncode == 0, case == "passing", result.stdout + result.stderr)
                calls = (fixture / "calls").read_text().splitlines()
                if case == "passing":
                    self.assertIn(f"test --locked -p awaken-execution-composition --lib {selected} -- --exact --list", calls)
                    self.assertIn(f"test --locked -p awaken-execution-composition --lib {selected} -- --exact --show-output", calls)
                    self.assertEqual(calls.count("E2E"), 5)
                else:
                    self.assertIn("required selection is empty", result.stderr)
                    self.assertNotIn("E2E", calls)

    @unittest.skipIf(os.name == "nt", "the Bash orchestration runs in the Linux gate")
    def test_actual_required_runner_rejects_empty_ignored_skipped_and_failed_runs(self) -> None:
        # E0 both orchestration fixtures consume the same complete deadline
        # input declaration, preserving the selection decision's prerequisites.
        # E1 same argv for listing/execution, including existing libtest args.
        # E2 zero selection, E3 ignored, E4 fixture skip, E5 execution failure,
        # E6 listing failure all fail. Preserve E6's status and never execute.
        for case in ("passing", "empty", "ignored", "skipped", "failed", "list-failed"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                self.runner_fixture(fixture)
                binary = fixture / "bin"
                binary.mkdir()
                cargo = binary / "cargo"
                cargo.write_text('''#!/usr/bin/env bash
if [ "$1" = awaken-ci-deadline ]; then shift 3; exec "$@"; fi
printf '%s\\n' "$*" >> "$CALL_LOG"
case "$*" in
  *--list*) [ "$CASE" = empty ] || echo 'critical: test'; [ "$CASE" != list-failed ] || exit 47; exit 0 ;;
esac
if [ "$CASE" = empty ]; then
  echo 'test result: ok. 0 passed; 0 failed; 0 ignored;'; exit 0
elif [ "$CASE" = ignored ]; then
  echo 'test critical ... ignored'; echo 'test result: ok. 0 passed; 0 failed; 1 ignored;'; exit 0
fi
echo 'test critical ... ok'
echo 'test result: ok. 1 passed; 0 failed; 0 ignored;'
[ "$CASE" != skipped ] || echo 'SKIPPED PostgreSQL fixture: absent'
[ "$CASE" != failed ]
''', encoding="utf-8")
                cargo.chmod(0o755)
                git = binary / "git"
                git.write_text("#!/bin/sh\n[ \"$1\" != rev-parse ] || echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n", encoding="utf-8")
                git.chmod(0o755)
                environment = dict(os.environ, PATH=f"{binary}:{os.environ['PATH']}", CASE=case, CALL_LOG=str(fixture / "calls"), CARGO_TARGET_DIR=str(fixture / "target"))
                result = subprocess.run(["bash", "-c", "source scripts/ci/_rust_test_execution.sh; run_required_rust_tests -p critical --lib critical -- --exact"], cwd=fixture, env=environment, capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode == 0, case == "passing", result.stdout + result.stderr)
                expected = ["test --locked -p critical --lib critical -- --exact --list"]
                if case == "list-failed":
                    self.assertEqual(result.returncode, 47)
                else:
                    expected.append("test --locked -p critical --lib critical -- --exact --show-output")
                self.assertEqual((fixture / "calls").read_text().splitlines(), expected)

    def test_selection_and_execution_decision_table(self) -> None:
        # R1 selected identities + actual successes => pass. R2 zero/renamed/
        # missing/duplicated tests, ignored/failure, false summary, fixture skip
        # or process failure => reject. Equal counts with different names fail.
        inventory = "tests::one: test\ntests::two: test\n2 tests, 0 benchmarks\n"
        output = "test tests::one ... ok\ntest tests::two ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored;\n"
        self.assertEqual(execution_errors(inventory, output, 0), [])
        variants = (
            ("", output, 0), (inventory, "", 0),
            (inventory, output.replace("tests::two ...", "tests::renamed ..."), 0),
            (inventory, output.replace("test tests::two ... ok\n", ""), 0),
            (inventory, output + "test tests::one ... ok\n", 0),
            (inventory, output.replace("two ... ok", "two ... ignored"), 0),
            (inventory, output.replace("two ... ok", "two ... FAILED"), 0),
            (inventory, output.replace("2 passed", "0 passed"), 0),
            (inventory, output.replace("0 ignored", "1 ignored"), 0),
            (inventory, output + "SKIPPED PostgreSQL fixture: absent\n", 0),
            (inventory, output + "[skip] no NATS reachable\n", 0),
            (inventory, output, 101),
        )
        for listed, result, status in variants:
            with self.subTest(listed=listed, result=result, status=status):
                self.assertTrue(execution_errors(listed, result, status))

    def test_same_name_in_two_binaries_retains_occurrence_count(self) -> None:
        inventory = "same: test\nsame: test\n"
        output = ("test same ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n") * 2
        self.assertEqual(execution_errors(inventory, output, 0), [])
        self.assertTrue(execution_errors(inventory, output[:len(output) // 2], 0))

    def test_missing_backend_is_rejected_even_when_every_listed_test_passes(self) -> None:
        inventory = "sqlite: test\n"
        output = "test sqlite ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
        self.assertEqual(execution_errors(inventory, output, 0, ["sqlite"]), [])
        self.assertTrue(execution_errors(inventory, output, 0, ["sqlite", "postgres"]))


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--inventory", type=Path)
    parser.add_argument("--results", type=Path)
    parser.add_argument("--exit-code", type=int)
    parser.add_argument("--require-case", action="append", default=[])
    parser.add_argument("--receipt", type=Path)
    args = parser.parse_args()
    if args.self_test:
        if args.inventory or args.results or args.exit_code is not None:
            parser.error("--self-test cannot be combined with execution arguments")
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ExecutionTests)
        return int(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
    if args.inventory is None or args.results is None or args.exit_code is None:
        parser.error("--inventory, --results and --exit-code are required")
    inventory = args.inventory.read_text(encoding="utf-8")
    results = args.results.read_text(encoding="utf-8")
    errors = execution_errors(inventory, results, args.exit_code, args.require_case)
    if args.receipt:
        from rust_test_plan import source_snapshot
        directory = args.receipt.parent
        before = (directory / "source.digest").read_text().strip()
        unchanged = before == source_snapshot()[0]
        if not unchanged:
            errors.append("source changed during required backend execution")
        args.receipt.write_text(json.dumps({
            "schema_version": 1, "kind": "required-libtest", "full_gate": False,
            "revision": (directory / "source.txt").read_text().strip(),
            "source_digest": before, "source_unchanged": unchanged,
            "arguments": (directory / "arguments.txt").read_text().splitlines(),
            "list_command": (directory / "list-command.txt").read_text().splitlines(),
            "test_command": (directory / "test-command.txt").read_text().splitlines(),
            "duration_seconds": int((directory / "duration-seconds.txt").read_text().strip()),
            "required_cases": args.require_case, "selected_cases": sorted(LISTED.findall(inventory)),
            "executed_cases": EXECUTED.findall(results), "command_exit_code": args.exit_code,
            "status": "failed" if errors else "passed", "errors": errors,
            "postgres_fixture_required": os.environ.get("AWAKEN_TEST_REQUIRE_POSTGRES") == "1",
            "postgres_fixture_configured": "AWAKEN_TEST_DATABASE_URL" in os.environ,
        }, indent=2) + "\n", encoding="utf-8")
    for error in errors:
        print(f"Rust execution: {error}", file=sys.stderr)
    if not errors:
        print(f"Rust execution: {len(LISTED.findall(args.inventory.read_text(encoding='utf-8')))} selected tests passed without skips")
    return int(bool(errors))


if __name__ == "__main__":
    raise SystemExit(main())
