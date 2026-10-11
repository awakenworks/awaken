#!/usr/bin/env python3
"""Compare ordinary cold/warm builds of an explicit Rust test workload.

Retain the original workload CLI and evidence comparison functions. Execution
is explicitly pending until whole-graph case projection is proved; it starts
no Cargo observations and owns no environment, source snapshot or runner.
"""
from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
from unittest import mock
import subprocess
import sys
import unittest

import crate_test
import nextest_execution

ROOT = Path(__file__).resolve().parents[2]
INPUT_GLOBS = ("scripts/ci/benchmark_rust_build.py", "scripts/ci/crate_test.py", "scripts/ci/nextest_execution.py", "scripts/ci/deadline.py",
               "scripts/ci/formal_sources.py", "scripts/ci/test.sh", ".config/rust-tests.toml")


def build_artifacts(text: str) -> tuple[int, dict[str, dict]]:
    compilations = 0
    artifacts = {}
    for line in text.splitlines():
        if not line.startswith("{"):
            continue
        record = json.loads(line)
        if record.get("reason") != "compiler-artifact":
            continue
        compilations += not record["fresh"]
        if record.get("executable") and record.get("profile", {}).get("test"):
            key = json.dumps({field: record[field] for field in
                              ("package_id", "target", "profile", "features", "filenames", "executable")}, sort_keys=True)
            if key in artifacts:
                raise ValueError("duplicate test artifact: " + key)
            path = Path(record["executable"])
            if not path.is_file() or not path.stat().st_size:
                raise ValueError("missing or empty test executable: " + str(path))
            if os.name == "nt" and (path.stat().st_size < 1024 or path.open("rb").read(2) != b"MZ"):
                raise ValueError("test output is not a native Windows executable: " + str(path))
            artifacts[key] = {"path": str(path), "bytes": path.stat().st_size,
                              "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
    if not artifacts:
        raise ValueError("the workload produced no test executables")
    return compilations, artifacts


def executed_cases(text: str) -> Counter[tuple[str, str]]:
    cases = Counter(nextest_execution.PASS.findall(text))
    summaries = nextest_execution.SUMMARY.findall(text)
    if len(summaries) != 1:
        raise ValueError("one complete Nextest summary is required")
    run, passed, skipped = (int(value or 0) for value in summaries[0])
    if not run or run != passed or skipped or sum(cases.values()) != run or any(count != 1 for count in cases.values()) or nextest_execution.SKIP.search(text):
        raise ValueError("all nonempty selected tests must execute and pass without skips")
    return cases


def positive(value: str) -> int:
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("a positive integer is required")
    return number


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("-p", "--package", action="append", default=[])
    parser.add_argument("--jobs", nargs="+", type=positive, default=[1, 2, 4])
    parser.add_argument("--rounds", type=positive, default=2)
    parser.add_argument("--timeout", type=positive, default=900)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(
            unittest.defaultTestLoader.loadTestsFromTestCase(BenchmarkTests)).wasSuccessful())
    if not args.package or len(set(args.package)) != len(args.package) or len(set(args.jobs)) != len(args.jobs):
        parser.error("packages and job counts must be nonempty and unique")
    print("Benchmark execution is pending complete whole-graph test/case projection; no observation started", file=sys.stderr)
    return 2


class BenchmarkTests(unittest.TestCase):
    def test_execution_evidence_rejects_empty_skipped_missing_and_incomplete_runs(self) -> None:
        good = " PASS [ 0.1s] (1/1) package target::case\n Summary [ 0.1s] 1 tests run: 1 passed, 0 skipped\n"
        self.assertEqual(executed_cases(good), Counter({("package", "target::case"): 1}))
        self.assertEqual(executed_cases(good.replace(", 0 skipped", "")), Counter({("package", "target::case"): 1}))
        for invalid in ("", good.replace("0 skipped", "1 skipped"),
                        good.replace("1 tests run", "0 tests run"), good.splitlines()[-1], good + good):
            with self.assertRaises(ValueError):
                executed_cases(invalid)

    def test_comparison_reuses_native_and_progress_output_and_refuses_fixture_skip(self):
        # B1 progress counters or native singular output + exact successful
        # summary -> same named case. B2 fixture skip, duplicate or empty
        # outcomes -> reject even with summary0. Neither form proves Source.
        for prefix in ("(1/1) ", ""):
            output = f"PASS [ 0.1s] {prefix}pkg::test critical\nSummary [ 0.1s] 1 test run: 1 passed\n"
            self.assertEqual(executed_cases(output), Counter({("pkg::test", "critical"): 1}))
            for invalid in (output + "SKIPPED PostgreSQL fixture: absent\n",
                            output + output.splitlines()[0] + "\n", output.replace("PASS", "FAIL")):
                with self.subTest(prefix=prefix, invalid=invalid), self.assertRaises(ValueError):
                    executed_cases(invalid)

    def test_ordinary_environment_uses_exact_owned_target_and_job_count(self) -> None:
        target = ROOT / "target/test-timings/probe"
        # N4/N13: target/BUILD comes from its existing owner; pending execution
        # does not add a second environment or remove compiler context silently.
        with mock.patch.dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_BUILD_JOBS="2", RUSTC_WRAPPER="caller-owned-wrapper"):
            result = crate_test.environment()
        self.assertEqual(result["CARGO_TARGET_DIR"], str(target))
        self.assertEqual(result["CARGO_BUILD_BUILD_DIR"], str(target))
        self.assertEqual(result["CARGO_BUILD_JOBS"], "2")
        self.assertEqual(result["RUSTC_WRAPPER"], "caller-owned-wrapper")

    def test_pending_workload_starts_no_observation_or_graph_read(self):
        # N4/N12-N15: selected workload is not proved for whole+E/no-skips.
        # Preserve the CLI and explicit pending failure; no metadata, cold
        # target, runner, changed profile or invented performance receipt.
        with mock.patch.object(sys, "argv", [__file__, "-p", "leaf", "--jobs", "1", "--rounds", "1"]), \
                mock.patch.object(subprocess, "run", side_effect=AssertionError("observation started")), \
                mock.patch.object(crate_test, "full_graph_metadata", side_effect=AssertionError("graph read started")):
            self.assertEqual(main(), 2)

    def test_artifact_projection_retains_unit_variants_and_rejects_missing_bytes(self):
        # N12: same package/target name x different feature/profile/path units.
        # Full identities must not collapse; exact duplicates/missing/empty
        # bytes reject. Comparison utilities have no execution/qualification role.
        import tempfile
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "test-executable"
            path.write_bytes(b"MZ" + b"x" * 2048)
            row = {"reason": "compiler-artifact", "fresh": False, "package_id": "selected",
                   "target": {"name": "same", "kind": ["lib"]}, "profile": {"test": True},
                   "features": [], "filenames": [str(path)], "executable": str(path)}
            second = dict(row, features=["rich"])
            count, artifacts = build_artifacts("\n".join(map(json.dumps, (row, second))))
            self.assertEqual((count, len(artifacts)), (2, 2))
            self.assertEqual({r["sha256"] for r in artifacts.values()}, {hashlib.sha256(path.read_bytes()).hexdigest()})
            with self.assertRaisesRegex(ValueError, "duplicate"):
                build_artifacts("\n".join(map(json.dumps, (row, row))))
            path.write_bytes(b"")
            with self.assertRaisesRegex(ValueError, "missing or empty"):
                build_artifacts(json.dumps(row))


if __name__ == "__main__":
    raise SystemExit(main())
