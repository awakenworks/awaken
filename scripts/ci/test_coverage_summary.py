#!/usr/bin/env python3
"""Coverage evidence counterexamples, including unmeasured branch coverage."""
from pathlib import Path
import unittest
from _strict_unittest import main

from coverage_summary import summarize


class CoverageTests(unittest.TestCase):
    def test_groups_union_without_double_counting_or_tests_inflating_production(self) -> None:
        root = Path("/repo")
        metadata = {"workspace_members": ["a", "empty"], "packages": [{"id": p, "name": p, "manifest_path": str(root / "crates" / p / "Cargo.toml")} for p in ("a", "empty")]}
        first = "SF:crates/a/src/lib.rs\nDA:1,1\nDA:2,0\nDA:3,10\nBRDA:1,0,0,1\nBRDA:1,0,1,-\nend_of_record\nSF:crates/a/tests/test.rs\nDA:1,20\nend_of_record\n"
        second = "SF:crates/a/src/lib.rs\nDA:1,4\nDA:2,1\nBRDA:1,0,0,4\nBRDA:1,0,1,1\nend_of_record\n"
        report = summarize(metadata, [first, second], root, {"crates/a/src/lib.rs": {3}})
        self.assertEqual(report[0]["lines"], {"covered": 2, "total": 2})
        self.assertEqual(report[0]["branches"], {"covered": 2, "total": 2})
        self.assertIsNone(report[1]["lines"])
        self.assertIsNone(report[1]["branches"])
        line_only = summarize(metadata, ["SF:crates/a/src/lib.rs\nDA:1,0\nend_of_record\n"], root, {})
        self.assertEqual(line_only[0]["lines"], {"covered": 0, "total": 1})
        self.assertIsNone(line_only[0]["branches"], "missing instrumentation is not zero branches")
        excluded_branch = summarize(metadata, [first], root, {"crates/a/src/lib.rs": {1}})
        self.assertIsNone(excluded_branch[0]["branches"])

    def test_external_paths_and_empty_corpus_are_not_workspace_evidence(self) -> None:
        root = Path("/repo")
        metadata = {"workspace_members": ["a"], "packages": [{"id": "a", "name": "a", "manifest_path": str(root / "crates/a/Cargo.toml")}]}
        report = summarize(metadata, ["SF:/other/crates/a/src/lib.rs\nDA:1,50\nend_of_record\n"], root, {})
        self.assertIsNone(report[0]["lines"])
        with self.assertRaises(ValueError):
            summarize({"workspace_members": [], "packages": []}, [], root, {})

    def test_cached_file_ownership_preserves_prefixes_nested_roots_and_order(self) -> None:
        # Decision table: O1 exact src prefix -> owned; O2 a similar directory
        # prefix -> unowned; O3 nested workspace src roots -> preserve every
        # matching owner's original counts; O4 duplicate LCOV groups -> max
        # hits, not an inflated denominator; O5 absent branch data -> None.
        root = Path("/repo")
        directories = {"nested": "crates/a/src/nested", "other": "crates/ab", "a": "crates/a"}
        metadata = {"workspace_members": list(directories), "packages": [
            {"id": name, "name": name, "manifest_path": str(root / directory / "Cargo.toml")}
            for name, directory in directories.items()]}
        document = "SF:crates/a/src/lib.rs\nDA:1,1\nend_of_record\nSF:crates/a/src/nested/src/lib.rs\nDA:1,0\nend_of_record\nSF:crates/ab_extra/src/lib.rs\nDA:1,9\nend_of_record\n"
        rows = summarize(metadata, [document, document], root, {})
        self.assertEqual([row["crate"] for row in rows], ["a", "nested", "other"])
        self.assertEqual(rows[0]["lines"], {"covered": 1, "total": 2})
        self.assertEqual(rows[1]["lines"], {"covered": 0, "total": 1})
        self.assertIsNone(rows[2]["lines"])
        self.assertTrue(all(row["branches"] is None for row in rows))


if __name__ == "__main__":
    main()
