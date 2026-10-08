#!/usr/bin/env python3
"""Qualify a required libtest run against its independently listed selection."""
from __future__ import annotations

import argparse
from collections import Counter
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


def execution_errors(inventory: str, results: str, exit_code: int) -> list[str]:
    planned = Counter(LISTED.findall(inventory))
    observed = EXECUTED.findall(results)
    actual = Counter(name for name, _ in observed)
    errors: list[str] = []
    if exit_code:
        errors.append(f"test command failed with status {exit_code}")
    if not planned:
        errors.append("required selection is empty")
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
    @unittest.skipIf(os.name == "nt", "the Bash orchestration runs in the Linux gate")
    def test_actual_required_runner_rejects_empty_ignored_skipped_and_failed_runs(self) -> None:
        # E1 same argv for listing/execution, including existing libtest args.
        # E2 zero selection, E3 ignored, E4 fixture skip, E5 nonzero exit all fail.
        root = Path(__file__).resolve().parents[2]
        for case in ("passing", "empty", "ignored", "skipped", "failed"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary)
                scripts = fixture / "scripts/ci"
                scripts.mkdir(parents=True)
                for name in ("_rust_test_execution.sh", "_deadline.sh", "deadline.py", "check_rust_test_execution.py"):
                    shutil.copyfile(root / "scripts/ci" / name, scripts / name)
                binary = fixture / "bin"
                binary.mkdir()
                cargo = binary / "cargo"
                cargo.write_text('''#!/usr/bin/env bash
if [ "$1" = awaken-ci-deadline ]; then shift 3; exec "$@"; fi
printf '%s\\n' "$*" >> "$CALL_LOG"
case "$*" in
  *--list*) [ "$CASE" = empty ] || echo 'critical: test'; exit 0 ;;
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
                self.assertEqual((fixture / "calls").read_text().splitlines(), ["test --locked -p critical --lib critical -- --exact --list", "test --locked -p critical --lib critical -- --exact --show-output"])

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


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--inventory", type=Path)
    parser.add_argument("--results", type=Path)
    parser.add_argument("--exit-code", type=int)
    args = parser.parse_args()
    if args.self_test:
        if args.inventory or args.results or args.exit_code is not None:
            parser.error("--self-test cannot be combined with execution arguments")
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ExecutionTests)
        return int(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
    if args.inventory is None or args.results is None or args.exit_code is None:
        parser.error("--inventory, --results and --exit-code are required")
    errors = execution_errors(args.inventory.read_text(encoding="utf-8"), args.results.read_text(encoding="utf-8"), args.exit_code)
    for error in errors:
        print(f"Rust execution: {error}", file=sys.stderr)
    if not errors:
        print(f"Rust execution: {len(LISTED.findall(args.inventory.read_text(encoding='utf-8')))} selected tests passed without skips")
    return int(bool(errors))


if __name__ == "__main__":
    raise SystemExit(main())
