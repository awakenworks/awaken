#!/usr/bin/env python3
"""Focused tests for the audited non-API reachability manifest."""

from __future__ import annotations

import importlib.util
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = pathlib.Path(__file__).with_name("check_changed_e2e_line_coverage.py")
SPEC = importlib.util.spec_from_file_location("changed_e2e_coverage", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class UnreachableManifestTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temporary.name)
        source = self.root / "crates/example/src/lib.rs"
        source.parent.mkdir(parents=True)
        source.write_text("one\ntwo\nthree\nfour\nfive\n", encoding="utf-8")
        self.previous_root = MODULE.ROOT
        MODULE.ROOT = self.root

    def tearDown(self) -> None:
        MODULE.ROOT = self.previous_root
        self.temporary.cleanup()

    def write_manifest(self, body: str) -> None:
        (self.root / "waivers.toml").write_text(body, encoding="utf-8")

    def test_loads_audited_ranges(self) -> None:
        self.write_manifest(
            """version = 1
[[waiver]]
path = "crates/example/src/lib.rs"
ranges = ["2-3", "5"]
reason = "This internal branch is not selected through the served API."
evidence = "cargo test -p example"
"""
        )
        by_path, entries = MODULE.unreachable_lines("waivers.toml")
        self.assertEqual(by_path["crates/example/src/lib.rs"], {2, 3, 5})
        self.assertEqual(len(entries), 1)

    def test_rejects_overlapping_or_out_of_bounds_ranges(self) -> None:
        self.write_manifest(
            """version = 1
[[waiver]]
path = "crates/example/src/lib.rs"
ranges = ["2-4", "4-6"]
reason = "This internal branch is not selected through the served API."
evidence = "cargo test -p example"
"""
        )
        with self.assertRaises(ValueError):
            MODULE.unreachable_lines("waivers.toml")

    def test_rejects_waivers_without_specific_evidence(self) -> None:
        self.write_manifest(
            """version = 1
[[waiver]]
path = "crates/example/src/lib.rs"
ranges = ["2"]
reason = "too short"
evidence = "test"
"""
        )
        with self.assertRaises(ValueError):
            MODULE.unreachable_lines("waivers.toml")

    def test_subprocess_output_is_always_decoded_as_utf8(self) -> None:
        # Cause graph: C1 repository output contains non-ASCII text; C2 the host
        # locale is not UTF-8. C1+C2 previously made the coverage gate fail before
        # reading counters. Explicit UTF-8 decoding makes both locale rules equal.
        #
        # | Rule | non-ASCII output | non-UTF-8 locale | Result |
        # |---|---|---|---|
        # | U1 | F | T | decoded |
        # | U2 | T | F | decoded |
        # | U3 | T | T | decoded as UTF-8 |
        def completed(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            self.assertEqual(kwargs.get("encoding"), "utf-8")
            self.assertNotIn("text", kwargs)
            return subprocess.CompletedProcess(args, 0, stdout="模型 ✅\n")

        with mock.patch.object(MODULE.subprocess, "run", side_effect=completed):
            self.assertEqual(MODULE.run("git", "diff"), "模型 ✅\n")

    def test_merges_homogeneous_lcov_groups_by_max_line_count(self) -> None:
        # Cause graph: C1/C2 are profiles from different feature/binary groups;
        # E1 their reports are exported independently (so LLVM never hash-merges
        # incompatible functions); E2 the gate keeps the maximum counter per
        # source line. This is equivalent to a union of served observations.
        #
        # | Rule | group A | group B | merged line 1 / line 2 |
        # |---|---:|---:|---:|
        # | L1 | 3 / 0 | 0 / 5 | 3 / 5 |
        source = (self.root / "crates/example/src/lib.rs").resolve()
        # Windows may expose the temp root through an 8.3 alias while resolving
        # the existing child to its long path. Mirror production's resolved ROOT.
        MODULE.ROOT = source.parents[3]
        (self.root / "a.lcov").write_text(
            f"SF:{source}\nDA:1,3\nDA:2,0\nend_of_record\n",
            encoding="utf-8",
        )
        (self.root / "b.lcov").write_text(
            f"SF:{source}\nDA:1,0\nDA:2,5\nend_of_record\n",
            encoding="utf-8",
        )

        merged = MODULE.lcov_lines(None, ["a.lcov", "b.lcov"])

        self.assertEqual(merged["crates/example/src/lib.rs"], {1: 3, 2: 5})


if __name__ == "__main__":
    unittest.main()
