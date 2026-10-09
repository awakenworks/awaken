#!/usr/bin/env python3
"""Adapt repository diagnostics to the host's sole build-artifact GC.

No deletion policy or filesystem candidate walk lives here. The host owner
alone plans, rechecks and removes compiler subtrees. An unavailable owner is an
explicit refusal, including on --apply; there is no fallback deletion path.
"""
from __future__ import annotations

import argparse
from contextlib import redirect_stdout, redirect_stderr
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import gate_tools

INPUT_GLOBS = ("scripts/ci/reclaim_build_space.py", "scripts/ci/gate_tools.py",
               "scripts/ci/gate_tools.json")


def invoke_owner(*, apply: bool, summary: bool, verbose: bool) -> int:
    tool = next(row for row in gate_tools.load()["tools"]
                if row["name"] == "awaken-worktree-gc")
    found = gate_tools.locate(tool)
    if not found.usable or not found.where:
        print("host build-artifact GC unavailable; no artifacts changed. "
              + tool["install"], file=sys.stderr)
        return 1
    with tempfile.TemporaryDirectory(prefix="awaken-gc-report-") as temporary:
        report = Path(temporary) / "owner.json"
        command = [found.where, "--apply" if apply else "--dry-run",
                   "--report", str(report)]
        try:
            result = subprocess.run(command, capture_output=True, text=True)
        except OSError as error:
            print(f"host GC could not execute: {error}; no fallback deletion", file=sys.stderr)
            return 1
        if result.returncode:
            print(result.stderr or result.stdout, file=sys.stderr, end="")
            return 128 - result.returncode if result.returncode < 0 else result.returncode
        try:
            observed = json.loads(report.read_text())
            if (observed["apply"] is not apply
                    or not isinstance(observed["results"], list)
                    or not isinstance(observed["disk_after"], dict)
                    or not isinstance(observed["finished"], (int, float))):
                raise ValueError("incomplete or mismatched owner report")
        except (OSError, ValueError, KeyError, TypeError) as error:
            print(f"host GC report unavailable: {error}; no fallback deletion", file=sys.stderr)
            return 1
        print(f"build-artifact GC owner: {found.where}")
        for disk, usage in observed["disk_after"].items():
            print(f"disk: {usage['free'] / 1024**3:.1f} GiB free at {disk}")
        rows = observed["results"]
        removed = [row for row in rows if "deleted" in row]
        planned = [row for row in rows if "bytes" in row and "skip" not in row and "error" not in row]
        print(f"host GC: {len(rows)} profiles observed; {len(planned)} planned; "
              f"{len(removed)} removed; mode={'apply' if apply else 'dry-run'}")
        if verbose and not summary:
            for row in rows:
                print(json.dumps(row, sort_keys=True))
        for disk in observed.get("warnings", []):
            print(f"host GC reserve remains unsatisfied at {disk}", file=sys.stderr)
        return 0


class AdapterTests(unittest.TestCase):
    # Cause/effect rules: A1 default/summary/verbose -> one real dry-run owner;
    # A2 --apply -> one real apply owner; A3 absent owner -> refusal, zero child;
    # A4 child nonzero -> exact status; A5 missing/malformed report -> refusal.
    # Constraint: these fixtures prove delegation, not GC safety. The host's
    # existing tests alone own candidate identity, references and deletion.
    def fixture(self, root: Path, *, status: int = 0, report: bool = True) -> Path:
        owner = root / "owner"
        owner.write_text(f"#!{sys.executable}\n" + """
import json,sys
from pathlib import Path
root=Path(__file__).parent
(root/'calls').write_text(json.dumps(sys.argv[1:]))
if REPORT:
    output=Path(sys.argv[sys.argv.index('--report')+1])
    output.write_text(json.dumps({'apply':'--apply' in sys.argv,'results':[],
        'disk_after':{'/':{'free':1024**3}},'finished':1,'warnings':[]}))
raise SystemExit(STATUS)
""".replace("REPORT", repr(report)).replace("STATUS", str(status)))
        owner.chmod(0o700)
        return owner

    def fixture_runner(self, owner: Path):
        # The external-owner protocol owns argv/report/status; Unix shebang
        # discovery is not a precondition of those cases. Launch this exact
        # owned Python fixture explicitly on every OS, retaining a real child.
        # Production invoke_owner still executes its declared command directly.
        original_run = subprocess.run

        def launch(command, **kwargs):
            self.assertEqual(command[0], str(owner))
            return original_run([sys.executable, *command], **kwargs)

        return patch.object(subprocess, "run", side_effect=launch)

    def test_only_canonical_owner_receives_preview_or_apply(self):
        for apply in (False, True):
            with self.subTest(apply=apply), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                sentinel = root / "source"
                sentinel.write_bytes(b"retain")
                owner = self.fixture(root)
                with patch.object(gate_tools, "locate",
                                  return_value=gate_tools.Found("present", str(owner))), \
                        self.fixture_runner(owner), \
                        redirect_stdout(io.StringIO()):
                    self.assertEqual(invoke_owner(apply=apply, summary=True, verbose=False), 0)
                args = json.loads((root / "calls").read_text())
                self.assertEqual(args[:2], ["--apply" if apply else "--dry-run", "--report"])
                self.assertEqual(sentinel.read_bytes(), b"retain", "A1/A2 source remains")

    def test_absent_owner_never_starts_a_child(self):
        with patch.object(gate_tools, "locate", return_value=gate_tools.Found("absent")), \
                patch.object(subprocess, "run") as run, redirect_stderr(io.StringIO()):
            self.assertEqual(invoke_owner(apply=True, summary=False, verbose=False), 1)
        run.assert_not_called()

    def test_owner_failure_and_missing_report_cannot_become_success(self):
        for status, report, expected in ((23, False, 23), (0, False, 1)):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as tmp:
                owner = self.fixture(Path(tmp), status=status, report=report)
                with patch.object(gate_tools, "locate",
                                  return_value=gate_tools.Found("present", str(owner))), \
                        self.fixture_runner(owner), \
                        redirect_stderr(io.StringIO()):
                    self.assertEqual(invoke_owner(apply=False, summary=False, verbose=False), expected)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--apply", action="store_true")
    mode.add_argument("--dry-run", action="store_true")
    parser.add_argument("--summary", action="store_true")
    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(AdapterTests))
        return 0 if result.wasSuccessful() else 1
    return invoke_owner(apply=args.apply, summary=args.summary, verbose=args.verbose)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
