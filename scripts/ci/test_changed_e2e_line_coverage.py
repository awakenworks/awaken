#!/usr/bin/env python3
"""Focused tests for the changed served-process E2E coverage authority."""

from __future__ import annotations

import importlib.util
import pathlib
import re
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

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", *args],
            cwd=self.root,
            check=True,
            encoding="utf-8",
            stdout=subprocess.PIPE,
        ).stdout

    def initialize_git(self) -> str:
        self.git("init", "-q")
        self.git("config", "user.name", "Coverage Test")
        self.git("config", "user.email", "coverage@example.invalid")
        self.git("add", ".")
        self.git("commit", "-qm", "baseline")
        return self.git("rev-parse", "HEAD").strip()

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
        # Constraint K1: every input is a separately exported homogeneous group
        # over the same source tree; cross-group reachability is authoritative,
        # so counters are max-merged rather than summed or overwritten.
        # Decision rule L1=C1+C2=>E1+E2. Coverage rationale: complementary
        # positive/zero counters exercise both max directions and reject a
        # last-report-wins merge without inventing execution counts.
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

    def test_one_diff_parser_separates_additions_from_git_moved_lines(self) -> None:
        # Causes: C1 ordinary additions; C2 bold/alternative cyan additions;
        # C3 deletion, context, and no-newline markers; C4 multiple files/hunks.
        # Effects: E1 every addition has its exact destination line; E2 only
        # cyan additions are moved; E3 C3 never creates a destination line.
        # Constraint K1: this parser consumes the one full-tree Git authority;
        # it never infers a move by comparing source text itself.
        #
        # | Rule | ordinary | cyan | structural marker | multi-file | Effect |
        # |---|---:|---:|---:|---:|---|
        # | P1 | T | F | F | F | E1 only |
        # | P2 | F | T | F | F | E1 + E2 |
        # | P3 | F | F | T | F | E3 |
        # | P4 | T | T | T | T | exact per-file E1-E3 |
        document = """\
\x1b[1mdiff --git a/crates/example/src/lib.rs b/crates/example/src/lib.rs\x1b[m
\x1b[1m--- a/crates/example/src/lib.rs\x1b[m
\x1b[1m+++ b/crates/example/src/lib.rs\x1b[m
\x1b[36m@@ -1,2 +1,3 @@\x1b[m
\x1b[31m-old\x1b[m
\x1b[32m+ordinary\x1b[m
\x1b[1;36m+\x1b[m\x1b[1;36mmoved exact\x1b[m
 context
\\ No newline at end of file
\x1b[36m@@ -9,0 +12,1 @@\x1b[m
\x1b[36m+\x1b[m\x1b[36mmoved alternative\x1b[m
\x1b[1mdiff --git a/crates/second/src/lib.rs b/crates/second/src/lib.rs\x1b[m
\x1b[1m--- a/crates/second/src/lib.rs\x1b[m
\x1b[1m+++ b/crates/second/src/lib.rs\x1b[m
\x1b[36m@@ -0,0 +1,3 @@\x1b[m
\x1b[32m+second ordinary\x1b[m
\x1b[1;36m+\x1b[m\x1b[1;36msecond moved\x1b[m
\x1b[32m+++ b/not-a-header-inside-source\x1b[m
\x1b[1mdiff --git a/crates/deleted/src/lib.rs b/crates/deleted/src/lib.rs\x1b[m
\x1b[1m--- a/crates/deleted/src/lib.rs\x1b[m
\x1b[1m+++ /dev/null\x1b[m
\x1b[36m@@ -1,1 +0,0 @@\x1b[m
\x1b[31m-deleted only\x1b[m
"""

        added, moved = MODULE.added_and_moved_lines_from_diff(document)

        self.assertEqual(
            added,
            {
                "crates/example/src/lib.rs": {1, 2, 12},
                "crates/second/src/lib.rs": {1, 2, 3},
            },
        )
        self.assertEqual(
            moved,
            {
                "crates/example/src/lib.rs": {2, 12},
                "crates/second/src/lib.rs": {2},
            },
        )

    def test_real_git_preserves_semantic_whitespace_and_excludes_moves(self) -> None:
        # Causes: C1 an exact cross-file block move; C2 the same move with one
        # uniform indentation delta; C3 a moved block whose string/SQL literal
        # changes internal whitespace; C4 an unrelated ordinary addition.
        # Effects: E1 C1/C2 leave the denominator; E2 C3's semantic line and C4
        # remain; E3 moved_excluded counts only Git-cyan production additions.
        # Constraint K1: `allow-indentation-change` may normalize leading
        # indentation only; whitespace inside a Rust string remains behavior.
        #
        # | Rule | exact move | indent-only | literal whitespace | unique add | Effect |
        # |---|---:|---:|---:|---:|---|
        # | G1 | T | F | F | F | E1 |
        # | G2 | F | T | F | F | E1 |
        # | G3 | F | F | T | F | E2 semantic line |
        # | G4 | F | F | F | T | E2 ordinary line |
        moves = self.root / "crates/moves/src"
        moves.mkdir(parents=True)
        exact = """pub fn exact_move() -> u64 {
    let alpha = 11111111111111111111_u64;
    alpha + 22222222222222222222_u64
}
"""
        indent_before = """mod old_indent {
    fn indentation_move() -> u64 {
        let alpha = 3333333333333333333_u64;
        alpha + 4444444444444444444_u64
    }
}
"""
        indent_after = """fn indentation_move() -> u64 {
    let alpha = 3333333333333333333_u64;
    alpha + 4444444444444444444_u64
}
"""
        semantic_before = """pub fn semantic_move() -> &'static str {
    let sql = "SELECT alpha beta FROM durable_table";
    sql
}
"""
        (moves / "source.rs").write_text(
            "pub fn source_anchor() {}\n\n"
            + exact
            + "\n"
            + indent_before
            + "\n"
            + semantic_before,
            encoding="utf-8",
        )
        (moves / "destination.rs").write_text(
            "pub fn destination_anchor() {}\n", encoding="utf-8"
        )
        ordinary = self.root / "crates/ordinary/src/lib.rs"
        ordinary.parent.mkdir(parents=True)
        ordinary.write_text("pub fn existing() {}\n", encoding="utf-8")
        baseline = self.initialize_git()

        semantic_after = semantic_before.replace("alpha beta", "alpha  beta")
        (moves / "source.rs").write_text("pub fn source_anchor() {}\n", encoding="utf-8")
        (moves / "destination.rs").write_text(
            "pub fn destination_anchor() {}\n\n"
            + exact
            + "\n"
            + indent_after
            + "\n"
            + semantic_after,
            encoding="utf-8",
        )
        with ordinary.open("a", encoding="utf-8") as handle:
            handle.write("pub fn unique_ordinary_addition() {}\n")

        added, moved = MODULE.added_and_moved_lines(baseline)
        changed, moved_excluded = MODULE.changed_lines(baseline, None)
        destination_lines = (moves / "destination.rs").read_text(encoding="utf-8").splitlines()
        semantic_line = destination_lines.index(
            '    let sql = "SELECT alpha  beta FROM durable_table";'
        ) + 1
        exact_line = destination_lines.index("pub fn exact_move() -> u64 {") + 1
        indent_line = destination_lines.index(
            "    let alpha = 3333333333333333333_u64;"
        ) + 1

        self.assertIn(exact_line, moved["crates/moves/src/destination.rs"])
        self.assertNotIn(exact_line, changed.get("crates/moves/src/destination.rs", set()))
        self.assertIn(indent_line, moved["crates/moves/src/destination.rs"])
        self.assertNotIn(indent_line, changed.get("crates/moves/src/destination.rs", set()))
        self.assertIn(semantic_line, added["crates/moves/src/destination.rs"])
        self.assertNotIn(semantic_line, moved.get("crates/moves/src/destination.rs", set()))
        self.assertIn(semantic_line, changed["crates/moves/src/destination.rs"])
        self.assertIn(2, changed["crates/ordinary/src/lib.rs"])
        expected_moved = sum(
            len(lines - MODULE.test_only_lines(path))
            for path, lines in moved.items()
            if path.startswith("crates/") and "/src/" in path and path.endswith(".rs")
        )
        self.assertEqual(moved_excluded, expected_moved)

    def test_real_git_rename_and_filters_share_the_single_added_set(self) -> None:
        # Causes: C1 a high-similarity rename plus one append; C2 an inline
        # cfg(test) addition; C3 a src/tests.rs addition; C4 an ignored path.
        # Effects: E1 only C1's append remains; E2 rename body is not re-counted;
        # E3 C2-C4 do not enter the changed production denominator.
        # Constraint K1: rename detection, additions, move colors, and filtering
        # all derive from the same full-tree Git diff snapshot.
        #
        # | Rule | rename append | inline test | tests.rs | ignored | Effect |
        # |---|---:|---:|---:|---:|---|
        # | R1 | T | F | F | F | E1 + E2 |
        # | R2 | F | T | F | F | E3 |
        # | R3 | F | F | T | F | E3 |
        # | R4 | F | F | F | T | E3 |
        rename = self.root / "crates/rename/src/original.rs"
        rename.parent.mkdir(parents=True)
        rename.write_text(
            "".join(f"pub const LINE_{number:02}: u64 = {number};\n" for number in range(1, 41)),
            encoding="utf-8",
        )
        filters = self.root / "crates/filter/src"
        filters.mkdir(parents=True)
        (filters / "lib.rs").write_text(
            "pub fn shipped() {}\n\n#[cfg(test)]\nmod tests {\n"
            "    #[test]\n    fn existing() {}\n}\n",
            encoding="utf-8",
        )
        (filters / "tests.rs").write_text("fn existing_fixture() {}\n", encoding="utf-8")
        (filters / "ignored.rs").write_text(
            "pub fn ignored_existing() {}\n", encoding="utf-8"
        )
        baseline = self.initialize_git()

        renamed = rename.with_name("renamed.rs")
        self.git(
            "mv",
            rename.relative_to(self.root).as_posix(),
            renamed.relative_to(self.root).as_posix(),
        )
        with renamed.open("a", encoding="utf-8") as handle:
            handle.write("pub const APPENDED_AFTER_RENAME: u64 = 41;\n")
        (filters / "lib.rs").write_text(
            "pub fn shipped() {}\n\n#[cfg(test)]\nmod tests {\n"
            "    #[test]\n    fn existing() {}\n\n"
            "    #[test]\n    fn added_inline() {}\n}\n",
            encoding="utf-8",
        )
        with (filters / "tests.rs").open("a", encoding="utf-8") as handle:
            handle.write("fn added_fixture() {}\n")
        with (filters / "ignored.rs").open("a", encoding="utf-8") as handle:
            handle.write("pub fn ignored_addition() {}\n")

        added, _ = MODULE.added_and_moved_lines(baseline)
        changed, moved_excluded = MODULE.changed_lines(
            baseline, re.compile(r"crates/filter/src/ignored\.rs")
        )

        self.assertEqual(added["crates/rename/src/renamed.rs"], {41})
        self.assertEqual(changed, {"crates/rename/src/renamed.rs": {41}})
        self.assertEqual(moved_excluded, 0)

    def test_moved_exclusion_count_uses_only_unignored_production_lines(self) -> None:
        # Causes: C1 a moved production line; C2 a moved inline-test line; C3 a
        # moved src/tests.rs line; C4 a moved line in an explicitly ignored file.
        # Effects: E1 only C1 increments moved_excluded; E2 all four lines leave
        # the production denominator for their respective authoritative reason.
        # Constraint K1: source/test/ignore classification precedes move counting,
        # so test scaffolding cannot inflate the reported production-move total.
        #
        # | Rule | production | inline test | tests.rs | ignored | Effect |
        # |---|---:|---:|---:|---:|---|
        # | X1 | T | F | F | F | E1 + E2 |
        # | X2 | F | T | F | F | E2 only |
        # | X3 | F | F | T | F | E2 only |
        # | X4 | F | F | F | T | E2 only |
        inline = self.root / "crates/inline/src/lib.rs"
        inline.parent.mkdir(parents=True)
        inline.write_text(
            "pub fn shipped() {}\n#[cfg(test)]\nmod tests {\n    fn fixture() {}\n}\n",
            encoding="utf-8",
        )
        tests_file = self.root / "crates/separate/src/tests.rs"
        tests_file.parent.mkdir(parents=True)
        tests_file.write_text("fn fixture() {}\n", encoding="utf-8")
        ignored = self.root / "crates/ignored/src/lib.rs"
        ignored.parent.mkdir(parents=True)
        ignored.write_text("pub fn ignored() {}\n", encoding="utf-8")
        classified = {
            "crates/example/src/lib.rs": {2},
            "crates/inline/src/lib.rs": {4},
            "crates/separate/src/tests.rs": {1},
            "crates/ignored/src/lib.rs": {1},
        }

        with (
            mock.patch.object(MODULE, "diff_base", return_value="baseline"),
            mock.patch.object(
                MODULE,
                "added_and_moved_lines",
                return_value=(classified, classified),
            ),
        ):
            changed, moved_excluded = MODULE.changed_lines(
                "main", re.compile(r"crates/ignored/src/lib\.rs")
            )

        self.assertEqual(changed, {})
        self.assertEqual(moved_excluded, 1)


if __name__ == "__main__":
    unittest.main()
