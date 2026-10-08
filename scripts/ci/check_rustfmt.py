#!/usr/bin/env python3
"""Check the complete workspace without exceeding Windows' command-line limit."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
# rustfmt receives expanded target paths, not just Cargo's short package names.
# Reserve space below CreateProcessW's 32767 UTF-16 character limit for its
# executable, edition and flags. Batching must never drop a workspace member.
PATH_BUDGET = 24000


def expanded_path_length(paths: set[str]) -> int:
    return len(subprocess.list2cmdline(sorted(paths)).encode("utf-16-le")) // 2


def windows_batches(metadata: dict) -> list[list[str]]:
    members = metadata["workspace_members"]
    if not members or len(set(members)) != len(members):
        raise ValueError("formatting requires a nonempty unique workspace")
    packages = {package["id"]: package for package in metadata["packages"]}
    if not set(members) <= packages.keys():
        raise ValueError("Cargo metadata omits a workspace member")
    batches: list[list[str]] = []
    batch: list[str] = []
    paths: set[str] = set()
    for package in sorted((packages[identity] for identity in members), key=lambda p: p["name"]):
        targets = {target["src_path"] for target in package["targets"]}
        if expanded_path_length(targets) > PATH_BUDGET:
            raise ValueError(f"{package['name']} alone exceeds the native rustfmt path budget")
        if batch and expanded_path_length(paths | targets) > PATH_BUDGET:
            batches.append(batch)
            batch, paths = [], set()
        batch.append(package["name"])
        paths.update(targets)
    batches.append(batch)
    return batches


def check_workspace(*, windows: bool, root: Path = ROOT, run=subprocess.run) -> int:
    if not windows:
        return run(["cargo", "fmt", "--all", "--", "--check"], cwd=root).returncode
    result = run(["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=root, stdout=subprocess.PIPE, text=True, encoding="utf-8")
    if result.returncode:
        return result.returncode
    try:
        batches = windows_batches(json.loads(result.stdout))
    except (KeyError, TypeError, ValueError) as error:
        print(f"cannot plan complete workspace formatting: {error}", file=sys.stderr)
        return 1
    for names in batches:
        command = ["cargo", "fmt", *(argument for name in names for argument in ("-p", name)), "--", "--check"]
        result = run(command, cwd=root)
        if result.returncode:
            print("formatting failed for: " + ", ".join(names), file=sys.stderr)
            return result.returncode
    print(f"rustfmt: {sum(map(len, batches))} workspace packages checked in {len(batches)} native batches")
    return 0


class FormatTests(unittest.TestCase):
    @staticmethod
    def metadata(count=40):
        packages = [{"id": str(i), "name": f"package-{i:03}", "targets": [{"src_path": "C:/workspace with spaces/" + "x" * 900 + f"/{i}/lib.rs"}]} for i in range(count)]
        return {"workspace_members": [p["id"] for p in packages], "packages": [*packages, {"id": "foreign", "name": "external", "targets": []}]}

    def test_expanded_paths_are_bounded_and_every_member_is_checked_once(self):
        # F1 >32767 characters after target expansion -> bounded batches;
        # F2 spaces/Unicode -> Windows UTF-16 quoting, not package-name length;
        # F3 a nonmember package -> excluded, while all members occur once.
        metadata = self.metadata()
        metadata["packages"][0]["targets"][0]["src_path"] += "😀" * 300
        batches = windows_batches(metadata)
        self.assertGreater(len(batches), 1)
        self.assertEqual([name for batch in batches for name in batch], [f"package-{i:03}" for i in range(40)])
        packages = {p["name"]: p for p in metadata["packages"]}
        for batch in batches:
            paths = [t["src_path"] for name in batch for t in packages[name]["targets"]]
            self.assertLessEqual(len(subprocess.list2cmdline(paths).encode("utf-16-le")) // 2, PATH_BUDGET)

    def test_incomplete_empty_and_oversized_plans_fail_before_formatting(self):
        for metadata in ({"workspace_members": [], "packages": []}, {"workspace_members": ["missing"], "packages": []}, {"workspace_members": ["a", "a"], "packages": []}):
            with self.assertRaises(ValueError):
                windows_batches(metadata)
        metadata = self.metadata(1)
        metadata["packages"][0]["targets"][0]["src_path"] = "x" * (PATH_BUDGET + 1)
        with self.assertRaises(ValueError):
            windows_batches(metadata)

    def test_metadata_and_middle_batch_failures_preserve_status(self):
        # F4 listing fails -> no formatter; F5 a later batch is dirty -> its
        # exact failure survives, even though the preceding batch passed.
        for metadata_status, format_status, expected in ((47, 0, 47), (0, 23, 23), (0, 0, 0)):
            calls = []
            def run(command, **kwargs):
                calls.append(command)
                if command[1] == "metadata":
                    return subprocess.CompletedProcess(command, metadata_status, json.dumps(self.metadata()))
                status = format_status if len(calls) == 3 else 0
                return subprocess.CompletedProcess(command, status)
            self.assertEqual(check_workspace(windows=True, run=run), expected)
            if metadata_status:
                self.assertEqual(len(calls), 1)
            elif format_status:
                self.assertEqual(len(calls), 3)
            else:
                selected = [command[i + 1] for command in calls[1:] for i, value in enumerate(command) if value == "-p"]
                self.assertEqual(selected, [f"package-{i:03}" for i in range(40)])

    def test_posix_retains_the_existing_all_format_scope_and_status(self):
        calls = []
        def run(command, **kwargs):
            calls.append(command)
            return subprocess.CompletedProcess(command, 23)
        self.assertEqual(check_workspace(windows=False, run=run), 23)
        self.assertEqual(calls, [["cargo", "fmt", "--all", "--", "--check"]])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(FormatTests))
        return 0 if result.wasSuccessful() else 1
    return check_workspace(windows=os.name == "nt")


if __name__ == "__main__":
    raise SystemExit(main())
