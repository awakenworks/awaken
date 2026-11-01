#!/usr/bin/env python3
"""Exercise the coverage gate's cache and failure boundaries through its shell entry."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from _strict_unittest import main


@unittest.skipIf(os.name == "nt", "Bash orchestration is qualified in the Linux gate")
class CombinedCoverageTests(unittest.TestCase):
    def exercise(self, case: str, arguments: list[str] | None = None, *, entry: str = "combined-coverage.sh") -> tuple[int, list[dict], bool, bool]:
        root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary)
            scripts = fixture / "scripts/ci"
            scripts.mkdir(parents=True)
            for name in ("combined-coverage.sh", "e2e-coverage.sh", "_provider_environment.sh", "_deadline.sh", "deadline.py"):
                shutil.copyfile(root / "scripts/ci" / name, scripts / name)
            (fixture / "e2e").mkdir()
            binary = fixture / "bin"
            binary.mkdir()
            target = fixture / "instrumented"
            target.mkdir()
            (target / "retained-build").write_text("cache")
            (target / "old.profraw").write_text("stale")
            driver = binary / "driver"
            driver.write_text(f"#!{os.sys.executable}\n" + '''
import json, os, pathlib, subprocess, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ['CALLS'], 'a') as output:
    output.write(json.dumps({'name':name,'args':args,'target':os.environ.get('CARGO_TARGET_DIR'),'build':os.environ.get('CARGO_BUILD_BUILD_DIR')})+'\\n')
case = os.environ['CASE']
target = pathlib.Path(os.environ['COV_DIR'])
if name == 'cargo':
    if args[0] == 'awaken-ci-deadline':
        raise SystemExit(subprocess.call(args[3:]))
    if args[:2] == ['llvm-cov','show-env']: print('export RUSTFLAGS=--cfg=instrumented')
    elif args[:2] == ['llvm-cov','clean']:
        (target/'old.profraw').unlink()
    elif args[0] == 'metadata': print('{}')
    elif args[:2] == ['nextest','run'] and case == 'rust-fails': raise SystemExit(31)
    elif args[0] == 'test' and case == 'doc-fails': raise SystemExit(32)
    elif args[0] == 'build' and case == 'build-fails': raise SystemExit(33)
elif name == 'git': print('a'*40)
elif name == 'node': print('file:///fixture/tsx.mjs')
elif name == 'npm' and case == 'e2e-fails': raise SystemExit(34)
elif name == 'python3':
    script = pathlib.Path(args[0]).name
    if script == 'rust_test_plan.py':
        if '--features' in args: print('cli/container-k8s,sandbox/hand')
        else:
            counter = target/'digest-calls'
            previous = int(counter.read_text()) if counter.exists() else 0
            counter.write_text(str(previous+1))
            print('changed' if previous and case == 'source-changes' else 'digest')
    elif script == 'check_changed_e2e_line_coverage.py' and '--print-ignore-filename-regex' in args: print('ignored')
    elif script == 'export_homogeneous_lcov.py':
        destination = pathlib.Path(args[args.index('--output-directory')+1])
        destination.mkdir()
        first = destination/'fixture.lcov'
        first.write_text('DA:1,1\\n')
        if case == 'export-fails-empty': raise SystemExit(37)
        if case == 'export-fails-partial':
            print(first)
            raise SystemExit(37)
        if case != 'export-empty':
            print(first)
            if case == 'export-multiple':
                second = destination/'second.lcov'
                second.write_text('DA:1,1\\n')
                print(second)
    elif script == 'coverage_summary.py':
        pathlib.Path(args[args.index('--output')+1]).write_text('{}')
    elif script == 'deadline.py':
        raise SystemExit(subprocess.call([os.environ['REAL_PYTHON'], *args]))
''', encoding="utf-8")
            driver.chmod(0o755)
            for name in ("cargo", "git", "node", "npm", "python3"):
                (binary / name).symlink_to(driver)
            environment = dict(os.environ, PATH=f"{binary}:{os.environ['PATH']}", COV_DIR=str(target), CARGO_TARGET_DIR=str(target), AWAKEN_COVERAGE_REAL="0", AWAKEN_COVERAGE_RESUME="0", CASE=case, CALLS=str(fixture / "calls"), REAL_PYTHON=os.sys.executable)
            result = subprocess.run(["bash", "scripts/ci/" + entry, *(arguments or [])], cwd=fixture, env=environment, capture_output=True, text=True, timeout=15)
            calls = [json.loads(line) for line in (fixture / "calls").read_text().splitlines()] if (fixture / "calls").exists() else []
            return result.returncode, calls, (target / "retained-build").is_file(), (target / "old.profraw").exists()

    def test_fresh_counters_preserve_cache_and_use_one_feature_graph(self) -> None:
        status, calls, retained, stale = self.exercise("passing")
        self.assertEqual(status, 0)
        self.assertTrue(retained)
        self.assertFalse(stale)
        cargo = [c for c in calls if c["name"] == "cargo"]
        self.assertIn(["llvm-cov", "clean", "--workspace", "--profraw-only", "--locked"], [c["args"] for c in cargo])
        for call in cargo:
            self.assertEqual(call["target"], call["build"])
            if call["args"][0] in {"metadata", "nextest", "test", "build"}:
                self.assertIn("--locked", call["args"])
                self.assertEqual(call["args"][call["args"].index("--features")+1], "cli/container-k8s,sandbox/hand")
        self.assertTrue(any(c["args"][0].endswith("coverage_summary.py") for c in calls if c["name"] == "python3"))

    def test_every_failed_stage_and_source_change_refuses_a_report(self) -> None:
        for case, expected in (("rust-fails",31), ("doc-fails",32), ("build-fails",33), ("e2e-fails",34), ("source-changes",1)):
            with self.subTest(case=case):
                status, calls, retained, _ = self.exercise(case)
                self.assertEqual(status, expected)
                self.assertTrue(retained)
                self.assertFalse(any(c["args"][0].endswith("coverage_summary.py") for c in calls if c["name"] == "python3"))

    def test_e2e_export_keeps_exact_paths_and_refuses_incomplete_exports(self) -> None:
        # C/E table, using the same original shell fixture (all tool IO mocked):
        # E1 exporter0+one path / E2 exporter0+two paths => exact repeated
        # --lcov-path values in original order. E3 nonzero+no stdout and E4
        # nonzero+partial stdout => exact status37, no terminal checker call.
        # E5 success+empty stdout => refusal1, no checker/ungrouped fallback.
        # These fixture LCOV bytes are not actual coverage measurements.
        def terminal_checks(calls: list[dict]) -> list[dict]:
            return [c for c in calls if c["name"] == "python3"
                    and c["args"][0].endswith("check_changed_e2e_line_coverage.py")
                    and "--print-ignore-filename-regex" not in c["args"]]

        for case, names in [("passing", ["fixture.lcov"]),
                            ("export-multiple", ["fixture.lcov", "second.lcov"])]:
            with self.subTest(case=case):
                status, calls, _, _ = self.exercise(case, entry="e2e-coverage.sh")
                self.assertEqual(status, 0)
                exporter = next(c for c in calls if c["name"] == "python3"
                                and c["args"][0].endswith("export_homogeneous_lcov.py"))
                directory = Path(exporter["args"][exporter["args"].index("--output-directory")+1])
                checks = terminal_checks(calls)
                self.assertEqual(len(checks), 1)
                args = checks[0]["args"]
                actual = [args[i+1] for i, value in enumerate(args) if value == "--lcov-path"]
                self.assertEqual(actual, [str(directory/name) for name in names])
        for case, expected in [("export-fails-empty", 37),
                               ("export-fails-partial", 37), ("export-empty", 1)]:
            with self.subTest(case=case):
                status, calls, _, _ = self.exercise(case, entry="e2e-coverage.sh")
                self.assertEqual(status, expected)
                self.assertEqual(terminal_checks(calls), [])

    def test_unknown_arguments_fail_before_work(self) -> None:
        status, calls, retained, stale = self.exercise("passing", ["--quick"])
        self.assertEqual(status, 2)
        self.assertEqual(calls, [])
        self.assertTrue(retained and stale)


if __name__ == "__main__":
    main()
