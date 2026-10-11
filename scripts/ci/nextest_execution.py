#!/usr/bin/env python3
"""Pure helpers for named Nextest inventory and outcome comparison."""
from __future__ import annotations

from collections import Counter
import json
import re
import sys
import unittest

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



class ExecutionTests(unittest.TestCase):
    @staticmethod
    def inventory():
        return {"test-count": 2, "rust-suites": {"pkg::test": {
            "binary-id": "pkg::test", "status": "listed", "testcases": {
                "critical": {"ignored": False, "filter-match": {"status": "matches"}},
                "slow": {"ignored": True, "filter-match": {"status": "matches"}}}}}}

    output = "PASS [ 0.1s] (1/1) pkg::test critical\nSummary [ 0.1s] 1 tests run: 1 passed, 1 skipped\n"

    def test_identity_summary_and_fixture_negative_controls(self):
        # Decision rules: exact named successes + one consistent summary + no
        # fixture skip + status0 -> accept comparison. Any failed command,
        # missing/renamed/duplicated success, wrong totals or fixture skip ->
        # reject. This pure comparison owns no execution or source qualification.
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



if __name__ == "__main__":
    if sys.argv[1:] != ["--self-test"]:
        raise SystemExit("usage: nextest_execution.py --self-test")
    raise SystemExit(int(not unittest.TextTestRunner(verbosity=2).run(
        unittest.defaultTestLoader.loadTestsFromTestCase(ExecutionTests)).wasSuccessful()))
