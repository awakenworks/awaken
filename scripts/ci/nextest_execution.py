#!/usr/bin/env python3
"""List, execute and reconcile a focused Nextest selection under one deadline."""
from __future__ import annotations

from collections import Counter
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
PASS = re.compile(r"^\s*PASS\s+\[[^]]+\]\s+(?:\([^)]*\)\s+)?(\S+)\s+(\S+)", re.MULTILINE)
SUMMARY = re.compile(r"Summary \[[^]]+\] (\d+) tests? run: (\d+) passed(?: \([^\n]*\))?(?:, (\d+) skipped)?\s*$", re.MULTILINE)
SKIP = re.compile(r"^\s*(?:SKIPPED\b|\[skip\])", re.MULTILINE | re.IGNORECASE)


def selection(document: dict) -> tuple[Counter, list[dict]]:
    cases = Counter()
    excluded = []
    for binary, suite in document["rust-suites"].items():
        if suite["binary-id"] != binary:
            raise ValueError("mismatched Nextest binary")
        if suite["status"] == "skipped" and not suite["testcases"]:
            excluded.append({"binary": binary, "reason": "binary-filter"})
            continue
        if suite["status"] != "listed":
            raise ValueError("unlisted Nextest binary with unexpected cases")
        for name, case in suite["testcases"].items():
            identity = (binary, name)
            match = case["filter-match"]["status"]
            if not isinstance(case["ignored"], bool) or match not in ("matches", "mismatch"):
                raise ValueError("unknown Nextest ignored/filter schema")
            if match == "matches" and not case["ignored"]:
                cases[identity] += 1
            elif match == "mismatch" or case["ignored"]:
                excluded.append({"binary": binary, "test": name,
                                 "reason": "ignored" if case["ignored"] else case["filter-match"]["reason"]})
            else:
                raise ValueError("unknown Nextest filter status: " + match)
    if not cases:
        raise ValueError("required Nextest selection is empty")
    if document["test-count"] != sum(cases.values()) + sum("test" in row for row in excluded):
        raise ValueError("Nextest inventory count differs from named cases")
    return cases, excluded


def reconcile(document: dict, output: str, status: int) -> dict:
    expected, excluded = selection(document)
    actual = Counter(PASS.findall(output))
    summaries = SUMMARY.findall(output)
    if status:
        raise ValueError(f"Nextest execution failed with status {status}")
    if actual != expected:
        raise ValueError(f"Nextest identities differ: missing={dict(expected-actual)}, unexpected={dict(actual-expected)}")
    if len(summaries) != 1:
        raise ValueError("one complete Nextest summary is required")
    run, passed, skipped = (int(value or 0) for value in summaries[0])
    excluded_count = sum("test" in row for row in excluded)
    if run != sum(expected.values()) or passed != run or skipped != excluded_count:
        raise ValueError("Nextest summary differs from the independently listed selection")
    if SKIP.search(output):
        raise ValueError("a selected test skipped its fixture or behavior")
    return {"executed_cases": sorted(expected.elements()), "executed_count": run,
            "excluded_cases": [row for row in excluded if "test" in row], "excluded_count": excluded_count,
            "excluded_binaries": [row for row in excluded if "test" not in row]}


def listing_arguments(arguments: list[str]) -> list[str]:
    """Preserve all build/selection options; omit only execution-only settings."""
    result = []
    iterator = iter(arguments)
    for argument in iterator:
        flag, equals, _ = argument.partition("=")
        if len(argument) > 2 and argument.startswith("-j"):
            continue
        if flag in ("--no-fail-fast", "--fail-fast"):
            continue
        if flag in ("--test-threads", "-j", "--failure-output"):
            if not equals:
                try:
                    next(iterator)
                except StopIteration as error:
                    raise ValueError("missing runtime option value: " + flag) from error
            continue
        result.append(argument)
    return result


def execute(command: list[str], environment: dict[str, str], directory: Path,
            timeout: float = 900, run=subprocess.run) -> tuple[int, list[dict]]:
    """No retry. Failed listing stops execution; deadline.py owns descendants."""
    if command[:3] != ["cargo", "nextest", "run"] or timeout <= 0:
        raise ValueError("a Nextest run and positive shared deadline are required")
    forbidden = ("--message-format", "--status-level", "--final-status-level", "--success-output",
                 "--run-ignored", "--no-capture", "--color", "--no-run", "--hide-progress-bar", "--retries")
    if any(arg.split("=", 1)[0] in forbidden for arg in command[3:]):
        raise ValueError("verified execution owns listing, ignored policy and output options")
    listing = [*command[:2], "list", *listing_arguments(command[3:]), "--message-format", "json"]
    testing = [*command, "--status-level", "pass", "--final-status-level", "fail",
               "--success-output", "immediate", "--color", "never", "--hide-progress-bar", "--retries", "0"]
    rows = []
    directory.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    inventory = None
    for phase, argv in (("list", listing), ("test", testing)):
        log = directory / (phase + ".log")
        errors = directory / (phase + ".stderr.log")
        remaining = timeout - (time.monotonic() - started)
        row = {"phase": phase, "command": argv, "log": str(log), "stderr": str(errors)}
        rows.append(row)
        if remaining <= 0:
            row.update(exit_code=124, error="shared selection/execution deadline exhausted")
            return 124, rows
        stage_started = time.monotonic()
        try:
            with log.open("w", encoding="utf-8") as stdout, errors.open("w", encoding="utf-8") as stderr:
                status = run([sys.executable, str(ROOT / "scripts/ci/deadline.py"), str(remaining),
                              "--", *argv], cwd=ROOT, env=environment, stdout=stdout, stderr=stderr).returncode
            row.update(exit_code=status, duration_seconds=time.monotonic()-stage_started)
            row["command_exit_code"] = status
            if status:
                return status, rows
            if phase == "list":
                inventory = json.loads(log.read_text(encoding="utf-8"))
                planned, excluded = selection(inventory)
                row.update(selected_cases=sorted(planned.elements()), excluded_cases=excluded)
            else:
                row.update(reconcile(inventory, log.read_text(encoding="utf-8") + "\n" +
                                     errors.read_text(encoding="utf-8"), status))
        except (OSError, ValueError, KeyError, TypeError) as error:
            row.update(error=str(error), exit_code=1)
            return 1, rows
    return 0, rows


class ExecutionTests(unittest.TestCase):
    @staticmethod
    def inventory():
        return {"test-count": 2, "rust-suites": {"pkg::test": {
            "binary-id": "pkg::test", "status": "listed", "testcases": {
                "critical": {"ignored": False, "filter-match": {"status": "matches"}},
                "slow": {"ignored": True, "filter-match": {"status": "matches"}}}}}}

    output = "PASS [ 0.1s] (1/1) pkg::test critical\nSummary [ 0.1s] 1 tests run: 1 passed, 1 skipped\n"

    def test_identity_summary_and_fixture_negative_controls(self):
        self.assertEqual(reconcile(self.inventory(), self.output, 0)["executed_count"], 1)
        for output, status in ((self.output, 23), (self.output.replace("critical", "renamed"), 0),
                               (self.output.replace("PASS", "FAIL"), 0),
                               (self.output.replace("1 passed", "0 passed"), 0),
                               (self.output.replace("1 skipped", "2 skipped"), 0),
                               (self.output + self.output.splitlines()[0] + "\n", 0),
                               (self.output + "SKIPPED PostgreSQL fixture: missing\n", 0)):
            with self.subTest(output=output, status=status), self.assertRaises(ValueError):
                reconcile(self.inventory(), output, status)

    def test_empty_unknown_and_wrong_count_inventories_fail(self):
        for change in ("empty", "unknown", "count", "unlisted"):
            document = self.inventory()
            suite = document["rust-suites"]["pkg::test"]
            if change == "empty":
                suite["testcases"]["critical"]["ignored"] = True
            elif change == "unknown":
                suite["testcases"]["critical"]["filter-match"]["status"] = "new"
            elif change == "count":
                document["test-count"] = 3
            else:
                suite["status"] = "skipped"
            with self.subTest(change=change), self.assertRaises(ValueError):
                selection(document)

    def test_same_name_in_different_binaries_is_not_interchangeable(self):
        document = self.inventory()
        suite = document["rust-suites"]["pkg::test"]
        suite["testcases"].pop("slow")
        document["rust-suites"]["pkg::other"] = {**suite, "binary-id": "pkg::other"}
        output = self.output.replace(", 1 skipped", "").replace("1 tests run: 1 passed", "2 tests run: 2 passed")
        output = output.splitlines()[0] + "\n" + output
        with self.assertRaises(ValueError):
            reconcile(document, output, 0)

    def test_filtered_binary_and_native_non_progress_singular_output(self):
        document = self.inventory()
        document["test-count"] = 1
        document["rust-suites"]["pkg::test"]["testcases"].pop("slow")
        document["rust-suites"]["pkg::excluded"] = {"binary-id": "pkg::excluded", "status": "skipped", "testcases": {}}
        output = "PASS [ 0.1s] pkg::test critical\nSummary [ 0.1s] 1 test run: 1 passed, 0 skipped\n"
        result = reconcile(document, output, 0)
        self.assertEqual(result["executed_count"], 1)
        self.assertEqual(result["excluded_count"], 0)
        self.assertEqual(result["excluded_binaries"], [{"binary": "pkg::excluded", "reason": "binary-filter"}])

    def test_runtime_options_do_not_change_the_independent_selection(self):
        selection = ["--locked", "-p", "pkg", "--features", "backend", "-E", "test(critical)"]
        for runtime in (["--no-fail-fast"], ["--test-threads", "2"], ["--test-threads=2"],
                        ["--failure-output", "immediate-final"], ["--fail-fast", "-j", "1"], ["-j2"]):
            self.assertEqual(listing_arguments([*selection, *runtime]), selection)
        with self.assertRaises(ValueError):
            listing_arguments(["--test-threads"])

    def test_actual_runner_refuses_failed_empty_and_changed_selections(self):
        # Run real child processes through the real deadline owner. The adapter
        # substitutes a fixture executable for Cargo, leaving argv/log handling
        # and the list/execute reconciliation unchanged on Windows and Unix.
        for case in ("pass", "empty", "list-failed", "renamed", "fixture-skip"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                fixture = directory / "cargo.py"
                inventory = self.inventory()
                output = self.output
                if case == "empty":
                    inventory["rust-suites"] = {}
                    inventory["test-count"] = 0
                if case == "renamed":
                    output = output.replace("critical", "renamed")
                if case == "fixture-skip":
                    output += "SKIPPED PostgreSQL fixture: absent\n"
                fixture.write_text("import sys\nif 'list' in sys.argv:\n print(" + repr(json.dumps(inventory)) +
                                   ")\n sys.exit(" + ("47" if case == "list-failed" else "0") +
                                   ")\nelse:\n print(" + repr(output) + ")\n", encoding="utf-8")
                calls = []
                def run(argv, **kwargs):
                    calls.append(argv)
                    separator = argv.index("--")
                    actual = [*argv[:separator+1], sys.executable, str(fixture), *argv[separator+2:]]
                    return subprocess.run(actual, **kwargs)
                import os
                status, rows = execute(["cargo", "nextest", "run", "--locked", "-p", "pkg"],
                                       dict(os.environ), directory / "logs", 20, run)
                self.assertEqual(status == 0, case == "pass", rows)
                self.assertEqual(len(calls), 1 if case in ("empty", "list-failed") else 2)
                if case == "list-failed":
                    self.assertEqual(status, 47)


if __name__ == "__main__":
    if sys.argv[1:] != ["--self-test"]:
        raise SystemExit("usage: nextest_execution.py --self-test")
    raise SystemExit(int(not unittest.TextTestRunner(verbosity=2).run(
        unittest.defaultTestLoader.loadTestsFromTestCase(ExecutionTests)).wasSuccessful()))
