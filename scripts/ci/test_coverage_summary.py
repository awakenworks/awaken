#!/usr/bin/env python3
"""Coverage evidence counterexamples, including unmeasured branch coverage."""
from pathlib import Path
import json
import sys
import tempfile
import unittest
from unittest.mock import patch
from _strict_unittest import main

import coverage_summary
from coverage_summary import production_exclusions, summarize


class CoverageTests(unittest.TestCase):
    def test_parent_test_cfg_excludes_helpers_and_production_reachability_wins(self) -> None:
        # | Rule | source ownership                    | effect                 |
        # | P1   | cfg(test)-only child + helper       | exclude the whole file |
        # | P2   | any(test, shipping feature) child  | retain production data |
        # | P3   | same file via test and prod paths  | production wins        |
        # | P4   | high-hit test helper + missed prod| cannot inflate coverage|
        # Use the existing Cargo module walker; file names/assertion counts do
        # not decide membership. The literal counts below are the LCOV oracle.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            crate = root / "crates/a"
            (crate / "src").mkdir(parents=True)
            manifest = crate / "Cargo.toml"
            manifest.write_text('[package]\nname="a"\nversion="0.1.0"\n[features]\nshipping=[]\n')
            (crate / "src/lib.rs").write_text('pub fn live() {}\n#[cfg(test)] mod hidden;\n#[cfg(any(test, feature="shipping"))] mod mixed;\nmod shared;\n#[cfg(test)] #[path="shared.rs"] mod probe;\n')
            (crate / "src/hidden.rs").write_text('fn fixture_helper() {}\n#[test]\nfn checks() {}\n')
            for name in ("mixed", "shared"):
                (crate / f"src/{name}.rs").write_text('pub fn live() {}\n')
            metadata = {"workspace_members": ["a"], "packages": [
                {"id": "a", "name": "a", "manifest_path": str(manifest)}]}
            files = {f"crates/a/src/{name}.rs" for name in ("lib", "hidden", "mixed", "shared")}
            excluded, unknown = production_exclusions(metadata, files, root)
            self.assertEqual(unknown, {})
            self.assertEqual(excluded["crates/a/src/hidden.rs"], {1, 2, 3})
            self.assertEqual(excluded["crates/a/src/mixed.rs"], set())
            self.assertEqual(excluded["crates/a/src/shared.rs"], set())
            document = ''.join(f'SF:crates/a/src/{name}.rs\nDA:1,{hits}\nend_of_record\n'
                               for name, hits in (("lib", 0), ("hidden", 999), ("mixed", 1), ("shared", 0)))
            rows = summarize(metadata, [document], root, excluded)
            self.assertEqual(rows[0]["lines"], {"covered": 1, "total": 3})
            # Executed negative control: the original per-file projection
            # misses the parent's cfg and falsely reports two covered of four.
            original = {path: coverage_summary.test_only_lines(path, root=root) for path in files}
            self.assertEqual(summarize(metadata, [document], root, original)[0]["lines"],
                             {"covered": 2, "total": 4})

    def test_unresolved_production_include_cannot_claim_a_numeric_percentage(self) -> None:
        # U1 an unresolved production include prevents a complete ownership
        # verdict. U2 LLVM still measured a line/branch. Effect: retain the raw
        # LCOV input, report both production counters as unknown with the cause;
        # neither a numeric percentage nor a zero may claim that missing scope.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            crate = root / "crates/a"
            (crate / "src").mkdir(parents=True)
            manifest = crate / "Cargo.toml"
            manifest.write_text('[package]\nname="a"\nversion="0.1.0"\n')
            (crate / "src/lib.rs").write_text('include!(generated_path!());\npub fn live() {}\n')
            metadata = root / "metadata.json"
            metadata.write_text(json.dumps({"workspace_members": ["a"], "packages": [
                {"id": "a", "name": "a", "manifest_path": str(manifest)}]}))
            lcov = root / "fixture.lcov"
            lcov.write_text('SF:crates/a/src/lib.rs\nDA:2,1\nBRDA:2,0,0,1\nend_of_record\n')
            output = root / "report.json"
            arguments = ["coverage_summary.py", "--metadata", str(metadata), "--lcov-path", str(lcov),
                         "--execution-scope", "rust-workspace", "--revision", "a" * 40,
                         "--source-digest", "fixture", "--output", str(output)]
            with patch.object(coverage_summary, "ROOT", root), patch.object(sys, "argv", arguments):
                self.assertEqual(coverage_summary.main(), 0)
            report = json.loads(output.read_text())
            self.assertIsNone(report["crates"][0]["lines"])
            self.assertIsNone(report["crates"][0]["branches"])
            self.assertIn("unresolved", report["crates"][0]["unmeasured_reason"])
            self.assertTrue(report["unresolved_production_includes"]["a"])

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
