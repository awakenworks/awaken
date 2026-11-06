#!/usr/bin/env python3
"""Run one required Rust test selection under nextest and qualify it from nextest's own reports.

Rule: a required run passes only when nextest exits 0 and its JUnit report records
every case nextest lists for the same selection exactly once, with no failure, error,
retry or skip element; no selected case is #[ignore]d; every --require-case ran; no
case printed a fixture-skip marker; and the source and fixture inputs did not change.
Why: an empty filter, an ignored case, a renamed test or a test that returns early
because its live fixture is absent would otherwise read as green.
Fix: run the case locally with `cargo nextest run -p <crate> <name> -- --exact`; make
its fixture available, or update the --require-case in the change that renamed it.
"""
from __future__ import annotations

import argparse
from collections import Counter
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import unittest
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
INPUT_GLOBS = ("scripts/ci/nextest_execution.py", ".config/nextest.toml")
PROFILE = "required"
# Tests print one of these and return early when their live fixture is absent.
SKIP = re.compile(r"^[ \t]*(?:skip(?:ped|ping)?\b|\[skip\])", re.MULTILINE | re.IGNORECASE)
# Capture, listing, profile and retry policy belong to this runner, not to a caller.
REFUSED = ("--nocapture", "--no-capture", "--list", "--format", "--message-format", "--no-tests",
           "--retries", "--profile", "-P", "--no-run", "--show-output", "--run-ignored")
RUNS_IGNORED = ("--ignored", "--include-ignored")


def required_command(arguments: list[str]) -> tuple[list[list[str]], list[str]]:
    """Project a `cargo test`-shaped argv onto nextest: ([build, list, run], required cases)."""
    required, front, tail, threads = [], [], [], []
    words, in_tail = iter(arguments), False
    for word in words:
        if word == "--" and not in_tail:
            in_tail = True
            continue
        flag = word.split("=", 1)[0]
        if flag in REFUSED:
            raise ValueError(f"required runs own capture, listing, profile and retries; remove {flag}")
        if flag == "--require-case" and not in_tail:
            if not word.partition("=")[2]:
                raise ValueError("--require-case needs a test name")
            required.append(word.partition("=")[2])
        elif flag == "--test-threads":
            threads = ["--test-threads", word.partition("=")[2] or next(words, "")]
        else:
            (tail if in_tail else front).append(word)
    selection = [*([] if "--locked" in front else ["--locked"]), "--profile", PROFILE, *front]
    tail = ["--", *tail] if tail else []
    # Listing every ignored case too is what lets the reader see an ignored case inside the filter.
    every = [] if set(RUNS_IGNORED) & set(tail) else ["--run-ignored", "all"]
    return [["cargo", "nextest", "run", "--no-run", *selection, *tail],
            ["cargo", "nextest", "list", "--message-format", "json", *every, *selection, *tail],
            ["cargo", "nextest", "run", "--no-tests=fail", *threads, *selection, *tail]], required


def qualify(listing: str, report: str, status: int, required=(), *,
            runs_ignored: bool = False, run_log: str | None = None) -> dict:
    """Judge one run from `nextest list` JSON and the JUnit report; the rule is the module's."""
    errors = [f"nextest exited with status {status}"] if status else []
    listed, ignored = Counter(), []
    for suite in json.loads(listing)["rust-suites"].values():
        for name, case in suite.get("testcases", {}).items():
            if case["filter-match"]["status"] == "matches":
                (ignored.append(name) if case["ignored"] and not runs_ignored else listed.update([name]))
    root = ET.fromstring(report)
    ran, output = Counter(), []
    for case in root.iter("testcase"):
        name = case.get("name", "")
        ran[name] += 1
        verdicts = sorted({child.tag for child in case} - {"system-out", "system-err", "properties"})
        text = "".join(child.text or "" for child in case if child.tag in ("system-out", "system-err"))
        errors += [f"{name}: {', '.join(verdicts)}"] * bool(verdicts)
        errors += [f"{name} printed a fixture-skip marker"] * bool(SKIP.search(text))
        output.append(text)
    if not ran:
        errors.append("no test ran: the selection is empty")
    if ignored:
        errors.append("selected tests are #[ignore]d: " + ", ".join(sorted(ignored)))
    if ran != listed:
        errors.append(f"nextest listed and ran different cases: missing={dict(listed - ran)}, "
                      f"unexpected={dict(ran - listed)}")
    if missing := sorted(set(required) - set(ran)):
        errors.append("required cases did not run: " + ", ".join(missing))
    if run_log is not None and (not root.get("uuid") or root.get("uuid") not in run_log):
        errors.append("the JUnit report does not belong to this run")
    return {"errors": errors, "selected_cases": sorted(ran.elements()), "output": "".join(output)}


def execution_inputs() -> dict:
    """Fixture identity and backend this run binds; they must not change during it."""
    kind = os.environ.get("AWAKEN_TEST_FIXTURE_KIND")
    owner = os.environ.get("AWAKEN_TEST_FIXTURE_OWNER")
    if (kind is None) != (owner is None):
        raise ValueError("native fixture kind and owner must be supplied together")
    if kind is not None and (kind not in ("postgres", "nats", "s3")
                             or not re.fullmatch(r"[A-Za-z0-9._:-]{1,160}", owner or "")):
        raise ValueError("native fixture subject is malformed")
    backend = os.environ.get("AWAKEN_CRASH_BACKEND") or None
    if backend not in (None, "postgres", "sqlite"):
        raise ValueError("unsupported crash-test backend selector")
    return {"fixture": {"kind": kind, "owner": owner} if kind is not None else None, "crash_backend": backend}


def _status(code: int) -> int:
    return code if code >= 0 else 128 - code


def run(arguments: list[str], evidence: Path) -> int:
    """List, run and qualify one selection; the receipt stays `running` if this process dies."""
    from crate_test import native_command
    from rust_test_plan import source_snapshot, working_subject

    (_, listing, command), required = required_command(arguments)
    if os.environ.get("NEXTEST_NO_CAPTURE") not in (None, "", "0"):
        raise ValueError("required runs need captured output to see fixture skips; unset NEXTEST_NO_CAPTURE")
    report = Path(os.environ["CARGO_TARGET_DIR"]) / "nextest" / PROFILE / "junit.xml"
    digest, facts = working_subject()
    inputs = execution_inputs()
    receipt = evidence / "results.json"
    document = {"schema_version": 2, "kind": "required-nextest", "status": "running",
                "revision": facts["revision"].decode().strip(), "source_digest": digest,
                "arguments": arguments, "required_cases": required, "commands": [listing, command],
                "execution_inputs": inputs,
                "postgres_fixture_required": os.environ.get("AWAKEN_TEST_REQUIRE_POSTGRES") == "1",
                "postgres_fixture_configured": "AWAKEN_TEST_DATABASE_URL" in os.environ,
                "s3_fixture_required": os.environ.get("AWAKEN_TEST_REQUIRE_S3") == "1",
                "s3_fixture_configured": all(os.environ.get(name) for name in (
                    "AWAKEN_TEST_S3_ENDPOINT", "AWAKEN_TEST_S3_ACCESS_KEY", "AWAKEN_TEST_S3_SECRET_KEY"))}
    receipt.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    log = evidence / "results.log"
    with log.open("w", encoding="utf-8") as output, (evidence / "list.json").open("w", encoding="utf-8") as inventory:
        status = _status(subprocess.run(native_command(listing), cwd=ROOT, stdout=inventory, stderr=output).returncode)
        report.unlink(missing_ok=True)
        if status == 0:
            status = _status(subprocess.run(native_command(command), cwd=ROOT, stdout=output,
                                            stderr=subprocess.STDOUT).returncode)
    if report.exists():
        report.replace(evidence / "junit.xml")
    try:
        observed = qualify((evidence / "list.json").read_text(encoding="utf-8"),
                           (evidence / "junit.xml").read_text(encoding="utf-8"), status, required,
                           runs_ignored=bool(set(RUNS_IGNORED) & set(command)),
                           run_log=log.read_text(encoding="utf-8", errors="replace"))
    except (OSError, ValueError, KeyError, TypeError, ET.ParseError) as error:
        observed = {"errors": [f"nextest left no readable list/JUnit evidence: {error}"], "selected_cases": []}
    errors = observed["errors"]
    unchanged = source_snapshot()[0] == digest
    if not unchanged:
        errors.append("source changed during the required run")
    if execution_inputs() != inputs:
        errors.append("fixture or backend changed during the required run")
    exit_code = status or int(bool(errors))
    document.update(status="failed" if exit_code else "passed", exit_code=exit_code, command_exit_code=status,
                    source_unchanged=unchanged, selected_cases=observed["selected_cases"], errors=errors)
    receipt.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    for error in errors:
        print(f"Rust execution: {error}", file=sys.stderr)
    if errors:
        print("Rust execution rule: " + __doc__.split("Rule: ", 1)[1], file=sys.stderr)
    else:
        print(f"Rust execution: {len(observed['selected_cases'])} selected tests passed without skips")
    return exit_code


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("mode", choices=("build", "run"), nargs="?")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--evidence", type=Path)
    argv = sys.argv[1:]
    split = argv.index("--") if "--" in argv else len(argv)
    args, arguments = parser.parse_args(argv[:split]), argv[split + 1:]
    if args.self_test:
        from _strict_unittest import observed_runner
        return int(not observed_runner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(QualifyTests)).wasSuccessful())
    if args.mode is None or not arguments or (args.mode == "run") != (args.evidence is not None):
        parser.error("use: build -- ARGS | run --evidence DIR -- ARGS | --self-test")
    try:
        if args.mode == "build":
            from crate_test import native_command
            return _status(subprocess.run(native_command(required_command(arguments)[0][0]), cwd=ROOT).returncode)
        return run(arguments, args.evidence)
    except ValueError as error:
        print(f"Rust execution: {error}", file=sys.stderr)
        return 2


def junit(*cases: str, uuid: str = "run-1") -> str:
    """A nextest-shaped JUnit report; each case is `name` or `name|<child xml>`."""
    rows = "".join(f'<testcase name="{name}" classname="pkg">{child}</testcase>'
                   for name, _, child in (case.partition("|") for case in cases))
    return (f'<?xml version="1.0"?><testsuites name="nextest-run" uuid="{uuid}" tests="{len(cases)}">'
            f'<testsuite name="pkg">{rows}</testsuite></testsuites>')


def listing(*cases: str, ignored: tuple[str, ...] = ()) -> str:
    """A `nextest list --message-format json` document with one unrelated, filtered-out case."""
    rows = {name: {"ignored": name in ignored, "filter-match": {"status": "matches"}} for name in cases}
    rows["unrelated"] = {"ignored": True, "filter-match": {"status": "mismatch", "reason": "string"}}
    return json.dumps({"rust-suites": {"pkg": {"testcases": rows}}})


class QualifyTests(unittest.TestCase):
    LISTED = listing("one", "two")
    PASSED = junit("one|<system-out>ok</system-out>", "two")

    def rejected(self, report: str, status: int = 0, required=("one",), listed: str = LISTED, **kw) -> list[str]:
        return qualify(listed, report, status, required, **kw)["errors"]

    def test_a_listed_run_with_every_required_case_passes(self) -> None:
        self.assertEqual(self.rejected(self.PASSED, run_log="Starting (run ID: run-1)"), [])

    def test_a_required_case_that_did_not_run_is_rejected(self) -> None:
        self.assertIn("required cases did not run: three", self.rejected(self.PASSED, required=("one", "three")))

    def test_a_skipped_or_ignored_required_case_is_rejected(self) -> None:
        self.assertIn("two: skipped", self.rejected(junit("one", "two|<skipped/>")))
        self.assertIn("selected tests are #[ignore]d: two",
                      self.rejected(junit("one"), listed=listing("one", "two", ignored=("two",))))
        self.assertEqual(self.rejected(junit("one", "two"), listed=listing("one", "two", ignored=("two",)),
                                       runs_ignored=True), [])

    def test_zero_selected_tests_are_rejected(self) -> None:
        self.assertIn("no test ran: the selection is empty", self.rejected(junit(), listed=listing(), required=()))

    def test_a_failed_case_is_rejected_even_when_a_pipe_reported_exit_zero(self) -> None:
        for verdict in ("failure", "error", "flakyFailure", "rerunFailure"):
            with self.subTest(verdict=verdict):
                self.assertIn(f"two: {verdict}", self.rejected(junit("one", f"two|<{verdict}/>"), status=0))
        self.assertIn("nextest exited with status 101", self.rejected(self.PASSED, status=101))

    def test_a_printed_fixture_skip_is_rejected_but_ordinary_words_are_not(self) -> None:
        for marker in ("SKIPPED PostgreSQL fixture: absent", "  skip: AWAKEN_TEST_DATABASE_URL unset",
                       "[skip] no NATS reachable", "Skipping live Worker: AWAKEN_K8S_E2E=1 is required"):
            with self.subTest(marker=marker):
                report = junit("one", f"two|<system-err>x\n{marker}\n</system-err>")
                self.assertIn("two printed a fixture-skip marker", self.rejected(report))
        for word in ("skipper owns a connection", "skipped_count=0", "diagnostic: SKIP is a label"):
            with self.subTest(word=word):
                self.assertEqual(self.rejected(junit("one", f"two|<system-out>{word}</system-out>")), [])

    def test_renamed_duplicated_or_unlisted_cases_are_rejected(self) -> None:
        for report in (junit("one", "renamed"), junit("one", "two", "two"), junit("one")):
            with self.subTest(report=report):
                self.assertTrue(any(e.startswith("nextest listed and ran") for e in self.rejected(report)))

    def test_a_stale_report_from_another_run_is_rejected(self) -> None:
        self.assertIn("the JUnit report does not belong to this run",
                      self.rejected(self.PASSED, run_log="Starting (run ID: run-2)"))

    def test_captured_output_is_returned_for_live_fixture_witnesses(self) -> None:
        self.assertEqual(qualify(self.LISTED, self.PASSED, 0)["output"], "ok")

    def test_projection_keeps_the_selection_and_moves_runner_options(self) -> None:
        (build, listed, command), required = required_command(
            ["-p", "pkg", "--lib", "tests::", "--require-case=tests::one", "--", "--exact", "--test-threads=1"])
        self.assertEqual(required, ["tests::one"])
        selection = ["--locked", "--profile", PROFILE, "-p", "pkg", "--lib", "tests::"]
        self.assertEqual(build, ["cargo", "nextest", "run", "--no-run", *selection, "--", "--exact"])
        self.assertEqual(listed, ["cargo", "nextest", "list", "--message-format", "json", "--run-ignored", "all",
                                  *selection, "--", "--exact"])
        self.assertEqual(command, ["cargo", "nextest", "run", "--no-tests=fail", "--test-threads", "1",
                                   *selection, "--", "--exact"])
        self.assertNotIn("--run-ignored", required_command(["-p", "pkg", "--", "--ignored"])[0][1])
        for refused in (["--", "--nocapture"], ["--no-capture"], ["--retries=2"], ["-P", "ci"], ["--require-case="]):
            with self.subTest(refused=refused), self.assertRaises(ValueError):
                required_command(["-p", "pkg", *refused])

    def run_fixture(self, status: int, digests: list[str]) -> tuple[int, dict]:
        """Drive run() with nextest replaced by a writer of its two reports; nothing is built."""
        import tempfile
        from unittest import mock
        import crate_test
        import rust_test_plan

        def nextest(command, stdout, **_):
            if command[2] == "list":
                stdout.write(listing("one"))
            else:
                report.parent.mkdir(parents=True, exist_ok=True)
                report.write_text(junit("one"), encoding="utf-8")
                stdout.write("Nextest run ID run-1 with nextest profile: required\n")
            stdout.flush()
            return subprocess.CompletedProcess(command, status)
        with tempfile.TemporaryDirectory() as directory:
            target, evidence = Path(directory) / "target", Path(directory) / "evidence"
            report = target / "nextest" / PROFILE / "junit.xml"
            evidence.mkdir()
            snapshots = iter(digests)
            with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": str(target)}), \
                    mock.patch.object(subprocess, "run", side_effect=nextest), \
                    mock.patch.object(crate_test, "native_command", side_effect=lambda command: command), \
                    mock.patch.object(rust_test_plan, "working_subject", return_value=("a", {"revision": b"r"})), \
                    mock.patch.object(rust_test_plan, "source_snapshot", side_effect=lambda: (next(snapshots), b"")):
                code = run(["-p", "pkg", "--require-case=one"], evidence)
            return code, json.loads((evidence / "results.json").read_text(encoding="utf-8"))

    def test_a_source_change_during_the_run_is_rejected_and_nextest_status_is_kept(self) -> None:
        self.assertEqual(self.run_fixture(0, ["a"])[1]["status"], "passed")
        code, receipt = self.run_fixture(0, ["b"])
        self.assertEqual((code, receipt["status"], receipt["source_unchanged"]), (1, "failed", False))
        self.assertIn("source changed during the required run", receipt["errors"])
        self.assertEqual(self.run_fixture(23, ["a"])[0], 23)

    def test_execution_inputs_refuse_partial_or_malformed_fixture_identity(self) -> None:
        from unittest import mock
        for values in ({"AWAKEN_TEST_FIXTURE_KIND": "postgres"}, {"AWAKEN_CRASH_BACKEND": "mysql"},
                       {"AWAKEN_TEST_FIXTURE_KIND": "redis", "AWAKEN_TEST_FIXTURE_OWNER": "o"}):
            with self.subTest(values=values), mock.patch.dict(os.environ, values, clear=True), \
                    self.assertRaises(ValueError):
                execution_inputs()


if __name__ == "__main__":
    raise SystemExit(main())
