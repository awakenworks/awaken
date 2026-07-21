#!/usr/bin/env python3
"""Focused tests for the audited non-API reachability manifest."""

from __future__ import annotations

import importlib.util
import pathlib
import tempfile
import unittest


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


if __name__ == "__main__":
    unittest.main()
