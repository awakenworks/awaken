#!/usr/bin/env python3
"""Report advisory file length diagnostics for source, config, and design files.

Rust source files are measured by what they are, not by how long they look:

* functional lines -- non-blank production lines, comments and `cfg(test)` code
  excluded -- are compared with RUST_FUNCTIONAL_WARNING_LIMIT and
  RUST_FUNCTIONAL_ERROR_LIMIT. Historical `awaken-allow(file-limit-ceiling): N`
  markers retain their recorded functional-line references;
* inline test lines (the file's `cfg(test)` code) are compared with TEST_ERROR_LIMIT, the
  room a separate test file gets, so tests can stay beside the code they verify;
* physical lines are compared with RUST_PHYSICAL_BACKSTOP.

A Rust file is a test file when its path says so or when its crate does not
compile it into production (a `#[cfg(test)] mod x;` file); test files keep the
physical test reference. Other languages keep the physical CODE_* references.

* `file-limit-ceiling`: numeric records in the first 5 lines after the opening
  formal header are compared with HEAD, including increases, new markers and
  stale records. Every line-count finding is a warning. Source selection,
  decoding and malformed Git/rename input retain their original failures.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import formal_sources

# Input set: `--staged` size-checks every staged file; the suffix tables below
# only choose which limit applies, never whether the file is read. The trigger
# is therefore the whole staged set.
INPUT_GLOBS: tuple[str, ...] = ("**/*",)

REPO_ROOT = Path(__file__).resolve().parent.parent.parent

CODE_WARNING_LIMIT = 1000
CODE_ERROR_LIMIT = 2000
DOC_LIMIT = 1200
CONFIG_LIMIT = 1200
# Test code once had no reference at all, so single files grew to 27k lines and became
# the most-rebased files in the tree (runtime-host host/tests.rs was touched by
# 46 of 500 commits). Keep the historical test reference distinct from production.
TEST_ERROR_LIMIT = 3000
RUST_FUNCTIONAL_WARNING_LIMIT = 800
RUST_FUNCTIONAL_ERROR_LIMIT = 1500
RUST_PHYSICAL_BACKSTOP = 4000
# A test file that was already over TEST_ERROR_LIMIT when the limit arrived
# carries its ceiling in its own first lines, as `awaken-allow(file-limit-ceiling): N`.
# The ceiling lives with the file, not in a path list here, so a split or a
# rename carries it (or drops it) in the same diff. The committed ceiling, a
# newly introduced record, and a marker left on a file back under the reference
# remain advisory comparisons.
CEILING_MARKER_SCAN_LINES = 5
# The opening `// @formal ...` lines of a source are the obligation ledger's
# file-head anchors (scripts/ci/formal_anchors.py): ledger metadata that moved
# out of formal/coverage.json into the file it describes, not code. They count
# toward no limit and do not push a ceiling marker out of the lines it is read
# from. Only the opening run is skipped; an anchor line anywhere else counts.
FORMAL_HEADER_LINE = re.compile(r"// @formal [^\n]*(?:\n|$)")

CODE_SUFFIXES = {
    ".rs",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    ".mjs",
    ".cjs",
    ".py",
    ".go",
    ".java",
    ".sh",
    ".rb",
    ".astro",
    ".vue",
    ".sql",
}
DOC_SUFFIXES = {".md", ".mdx"}
CONFIG_SUFFIXES = {".json", ".toml", ".yaml", ".yml"}
LOCKFILES = {
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
}
EXCLUDED_PARTS = {
    ".git",
    ".next",
    ".turbo",
    ".vite",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "target",
}
TEST_PATH_RE = re.compile(r"(^|/)(fixtures|test-data|snapshots|tests|test)/")
TEST_FILE_RE = re.compile(
    r"(^|/)[^/]*(\.test|\.spec)\.(ts|tsx|js|jsx)$|(^|/)tests\.rs$|_test\.(go|rs)$"
)


_PRODUCTION_FILES: dict[Path, frozenset[Path]] = {}


def _workspace():
    sys.path.insert(0, str(REPO_ROOT / "scripts" / "ci"))
    import _crate_boundary_workspace

    return _crate_boundary_workspace


def _crate_manifest(path: Path) -> Path | None:
    for parent in path.parents:
        manifest = parent / "Cargo.toml"
        if manifest.is_file() and "[package]" in manifest.read_text(encoding="utf-8", errors="replace"):
            return manifest
        if parent == REPO_ROOT:
            return None
    return None


def compiled_into_production(rel: str) -> bool:
    """Whether the file's crate compiles it into its production build."""
    path = (REPO_ROOT / rel).resolve()
    manifest = _crate_manifest(path)
    if manifest is None:
        return False
    if manifest not in _PRODUCTION_FILES:
        _PRODUCTION_FILES[manifest] = frozenset(
            Path(item).resolve() for item in _workspace().production_module_files([manifest])
        )
    return path in _PRODUCTION_FILES[manifest]


def rust_measures(text: str) -> tuple[int, int, int]:
    """(functional, inline test, physical) lines of one Rust source."""
    workspace = _workspace()
    functional = sum(1 for line in workspace.production_rust(text).splitlines() if line.strip())
    code = sum(1 for line in workspace.rust_without_comments(text).splitlines() if line.strip())
    return functional, code - functional, count_lines(text)


def functional_lines(text: str) -> int:
    return rust_measures(text)[0]


def is_rust_source(rel: str) -> bool:
    return (Path(rel).suffix == ".rs" and not is_excluded(rel) and not is_test_path(rel)
            and compiled_into_production(rel))


def is_excluded(rel: str) -> bool:
    parts = set(rel.split("/"))
    if parts & EXCLUDED_PARTS:
        return True
    return "/generated/" in f"/{rel}/" or ".generated." in rel


def is_test_path(rel: str) -> bool:
    return bool(TEST_PATH_RE.search(rel) or TEST_FILE_RE.search(rel))


def limit_for(rel: str) -> int | None:
    path = Path(rel)
    if path.name in LOCKFILES or is_excluded(rel):
        return None
    if path.suffix in CODE_SUFFIXES:
        if is_test_path(rel):
            return TEST_ERROR_LIMIT
        return CODE_ERROR_LIMIT
    if path.suffix in DOC_SUFFIXES:
        return DOC_LIMIT
    if path.suffix in CONFIG_SUFFIXES:
        return CONFIG_LIMIT
    return None


# rustfmt runs as its own pre-commit command, in parallel with this one, and
# restages what it rewrites (`stage_fixed`). Judging the staged bytes therefore
# measured a file rustfmt was about to reflow: an expression broken over more
# lines pushed the committed file past its limit, and the *next* commit to touch
# it failed. Lefthook's parallel run cannot order the two without serialising
# every check, so a staged Rust file is measured as rustfmt will leave it.
def rustfmt_output(rel: str, text: str) -> str | None:
    """`text` as the rustfmt hook leaves `rel`, or None when rustfmt cannot format it.

    rustfmt reads its configuration (edition, newline style) from the
    directory it starts in, so it starts where the file lives. None -- rustfmt
    absent or the source unparsable -- keeps the staged bytes: the rustfmt hook
    refuses that commit itself.
    """
    directory = (REPO_ROOT / rel).parent
    try:
        result = subprocess.run(
            ["rustfmt", "--emit", "stdout"],
            input=text.encode("utf-8"),
            cwd=directory if directory.is_dir() else REPO_ROOT,
            capture_output=True,
            check=False,
        )
    except OSError:
        return None
    if result.returncode != 0:
        return None
    return result.stdout.decode("utf-8", errors="replace")


def as_committed(texts: dict[str, str], formatter=rustfmt_output) -> dict[str, str]:
    """Staged texts as the commit will hold them: Rust sources after rustfmt."""
    rust = [rel for rel in texts if rel.endswith(".rs")]
    with ThreadPoolExecutor(max_workers=max(1, min(len(rust), os.cpu_count() or 1))) as pool:
        formatted = dict(zip(rust, pool.map(lambda rel: formatter(rel, texts[rel]), rust)))
    return {rel: formatted.get(rel) or text for rel, text in texts.items()}


def without_formal_header(text: str) -> str:
    position = 0
    while (match := FORMAL_HEADER_LINE.match(text, position)) is not None and match.end() > position:
        position = match.end()
    return text[position:]


def count_lines(text: str) -> int:
    text = without_formal_header(text)
    if not text:
        return 0
    return text.count("\n") + (0 if text.endswith("\n") else 1)


def is_code_file(rel: str) -> bool:
    return Path(rel).suffix in CODE_SUFFIXES and not is_test_path(rel)


def warning_limit_for(rel: str) -> int | None:
    return CODE_WARNING_LIMIT if is_code_file(rel) and limit_for(rel) is not None else None


def classify_line_count(rel: str, lines: int) -> str | None:
    limit = limit_for(rel)
    if limit is None:
        return None
    if is_code_file(rel):
        if lines >= limit:
            return "warning"
        warning_limit = warning_limit_for(rel)
        if warning_limit is not None and lines >= warning_limit:
            return "warning"
        return None
    return "warning" if lines > limit else None


def is_test_code_file(rel: str) -> bool:
    return Path(rel).suffix in CODE_SUFFIXES and is_test_path(rel)


def declared_ceiling(text: str, *, historical: bool = False) -> int | None:
    for line in without_formal_header(text).splitlines()[:CEILING_MARKER_SCAN_LINES]:
        if historical:
            # Only the committed prior schema is normalized. Current sources
            # have one syntax; both old and later new HEADs use this one decoder
            # so the original held N never becomes measure(HEAD)+1 at cutover.
            line = line.replace("awaken-file-limit-ceiling:", "awaken-allow(file-limit-ceiling):", 1)
        reason = formal_sources.allow_reason(line, "file-limit-ceiling")
        match = re.match(r"(\d+)", reason) if reason is not None else None
        if match:
            return int(match.group(1))
    return None


def test_file_warnings(rel: str, text: str, previous: str | None, staged: bool) -> list[str]:
    """Test-file line diagnostics, with the recorded ceiling compared against HEAD."""
    return ceiling_warnings(
        rel, text, previous, staged, count_lines, TEST_ERROR_LIMIT, "lines", "test limit",
        "split the file by the behaviour its tests verify",
        "review the file's test responsibilities when adding coverage",
    )


def ceiling_warnings(rel: str, text: str, previous: str | None, staged: bool, measure, limit: int,
                   unit: str, label: str, split_advice: str, grow_advice: str,
                   *, measured: int | None = None) -> list[str]:
    """Compare line measurements and historical numeric records without rejecting them."""

    lines = measure(text) if measured is None else measured
    ceiling = declared_ceiling(text)
    warnings: list[str] = []
    if ceiling is None:
        if lines > limit:
            warnings.append(f"{rel}: {lines} {unit} > {label} {limit}; {split_advice}")
        return warnings
    if lines <= limit:
        warnings.append(
            f"{rel}: {lines} {unit} is within the {label} {limit}; remove its "
            "stale awaken-allow(file-limit-ceiling) marker"
        )
        return warnings
    if lines > ceiling:
        warnings.append(f"{rel}: {lines} {unit} > its awaken-allow(file-limit-ceiling) {ceiling}; {grow_advice}")
    if staged:
        if previous is None:
            warnings.append(
                f"{rel}: a new file declares awaken-allow(file-limit-ceiling) {ceiling}; reference {limit} {unit}"
            )
        else:
            before = declared_ceiling(previous, historical=True)
            previous_lines = measure(previous) if before is None else None
            bound = before if before is not None else previous_lines + 1
            if before is None and previous_lines <= limit:
                warnings.append(
                    f"{rel}: declares awaken-allow(file-limit-ceiling) {ceiling} but was within the {label} "
                    f"{limit}; the new numeric record does not change the measurement"
                )
            elif ceiling > bound:
                warnings.append(
                    f"{rel}: raises awaken-allow(file-limit-ceiling) to {ceiling} from {bound}; compare the recorded references"
                )
    return warnings


def rust_source_results(rel: str, text: str, previous: str | None, staged: bool) -> tuple[list[str], list[str]]:
    """Advisory measurements for one production Rust source; source errors still raise."""
    functional, inline_tests, physical = rust_measures(text)
    warnings = ceiling_warnings(
        rel, text, previous, staged, functional_lines, RUST_FUNCTIONAL_ERROR_LIMIT, "functional lines",
        "functional limit", "split the module by responsibility",
        "review the module's responsibilities when adding behaviour",
        measured=functional,
    )
    if inline_tests > TEST_ERROR_LIMIT:
        warnings.append(f"{rel}: {inline_tests} inline test lines > test limit {TEST_ERROR_LIMIT}; "
                      "split the tests by the behaviour they verify")
    if physical > RUST_PHYSICAL_BACKSTOP:
        warnings.append(f"{rel}: {physical} lines > backstop {RUST_PHYSICAL_BACKSTOP}; split the file")
    if not warnings and RUST_FUNCTIONAL_WARNING_LIMIT <= functional < RUST_FUNCTIONAL_ERROR_LIMIT:
        warnings.append(
            f"{rel}: {functional} functional lines >= warning threshold {RUST_FUNCTIONAL_WARNING_LIMIT}"
        )
    return warnings, []


def rename_sources(fields: list[str]) -> dict[str, str]:
    """New path -> committed path from `git diff --name-status -z -M --diff-filter=R` fields
    (`R<score>`, old, new triples), so a moved file is judged against the file it was."""
    if len(fields) % 3:
        raise ValueError("rename status fields must come in (status, old, new) triples")
    renames: dict[str, str] = {}
    for status, old, new in zip(fields[::3], fields[1::3], fields[2::3]):
        if not status.startswith("R"):
            raise ValueError(f"unexpected rename status {status!r}")
        renames[new] = old
    return renames


def staged_rename_sources() -> dict[str, str]:
    return rename_sources(formal_sources.git_paths(
        REPO_ROOT, "diff", "--cached", "--name-status", "-z", "-M", "--diff-filter=R"))


def find_results(files: list[str], staged: bool,
                 formatter=rustfmt_output) -> tuple[list[str], list[str], bool]:
    """Use one measurement path; numeric findings warn, source/protocol errors raise.

    Retain the existing three-part result for callers of this guard.
    """
    errors: list[str] = []
    warnings: list[str] = []
    has_code_violation = False
    texts = {rel: text for rel in sorted(files)
             if limit_for(rel) is not None and (text := formal_sources.file_text(REPO_ROOT, rel, staged)) is not None}
    renamed_from: dict[str, str] = {}
    if staged:
        texts = as_committed(texts, formatter)
        renamed_from = staged_rename_sources()
    for rel, text in texts.items():
        limit = limit_for(rel)
        if is_rust_source(rel):
            previous = formal_sources.committed_text(REPO_ROOT, renamed_from.get(rel, rel)) if staged else None
            found_warnings, found = rust_source_results(rel, text, previous, staged)
            warnings.extend(found_warnings)
            errors.extend(found)
            has_code_violation = has_code_violation or bool(found)
            continue
        if is_test_code_file(rel) or Path(rel).suffix == ".rs":
            previous = formal_sources.committed_text(REPO_ROOT, renamed_from.get(rel, rel)) if staged else None
            warnings.extend(test_file_warnings(rel, text, previous, staged))
            continue
        lines = count_lines(text)
        classification = classify_line_count(rel, lines)
        if classification == "warning":
            if is_code_file(rel):
                if lines >= limit:
                    warnings.append(f"{rel}: {lines} lines >= reference limit {limit}")
                else:
                    warnings.append(f"{rel}: {lines} lines >= warning threshold {CODE_WARNING_LIMIT}")
            else:
                warnings.append(f"{rel}: {lines} lines > reference limit {limit}")
    return warnings, errors, has_code_violation


def self_test() -> int:
    _selftest_source_ceilings()
    failures: list[str] = []
    cases = [
        ("src/lib.rs", CODE_ERROR_LIMIT),
        ("docs/design/runtime.md", DOC_LIMIT),
        ("package.json", CONFIG_LIMIT),
        ("tests/runtime.rs", TEST_ERROR_LIMIT),

        ("src/generated/schema.rs", None),
        ("Cargo.lock", None),
    ]
    for rel, expected in cases:
        actual = limit_for(rel)
        if actual != expected:
            failures.append(f"{rel}: expected {expected}, got {actual}")
    if count_lines("a\nb\n") != 2 or count_lines("a\nb") != 2:
        failures.append("line counter returned an unexpected count")
    # F1 opening ledger anchors do not count; F2 an anchor after code counts;
    # F3 a ceiling marker below the anchors is still read.
    anchors = "// @formal evidence run.safe\n// @formal entrypoint run.other\n"
    if count_lines(anchors + "a\nb\n") != 2 or count_lines("a\n" + anchors) != 3:
        failures.append("F1/F2 opening @formal anchors are metadata; later ones are lines")
    if declared_ceiling(anchors * 3 + "// awaken-allow(file-limit-ceiling): 3005\n") != 3005:
        failures.append("F3 a ceiling marker below the @formal anchors must still be read")
    line_cases = [
        ("src/lib.rs", CODE_WARNING_LIMIT - 1, None),
        ("src/lib.rs", CODE_WARNING_LIMIT, "warning"),
        ("src/lib.rs", CODE_ERROR_LIMIT - 1, "warning"),
        ("src/lib.rs", CODE_ERROR_LIMIT, "warning"),
        ("docs/design/runtime.md", DOC_LIMIT, None),
        ("docs/design/runtime.md", DOC_LIMIT + 1, "warning"),
        ("package.json", CONFIG_LIMIT + 1, "warning"),
    ]
    for rel, lines, expected in line_cases:
        actual = classify_line_count(rel, lines)
        if actual != expected:
            failures.append(f"{rel} at {lines} lines: expected {expected}, got {actual}")
    warnings, errors, has_code_error = find_results(["src/lib.rs", "src/large.rs"], staged=False)
    if warnings or errors or has_code_error:
        failures.append("empty fixture paths should not produce warnings/errors")
    if warning_limit_for("src/lib.rs") != CODE_WARNING_LIMIT:
        failures.append("source files should receive a 1000-line warning threshold")
    if warning_limit_for("tests/runtime.rs") is not None:
        failures.append("test files should not receive a source warning threshold")
    # Cause/effect table for test files. Causes: C1 lines vs limit, C2 marker
    # present, C3 lines vs marker, C4 staged against HEAD (absent / unmarked
    # under limit / unmarked over limit / marked). Effects: E1 no finding,
    # E2 advisory warning. Every original numeric/marker partition is retained.
    big = "// awaken-allow(file-limit-ceiling): 3005\n" + "x\n" * 3000
    small = "x\n" * 10
    rows = [
        ("T1 under the limit, no marker", small, None, False, False),
        ("T2 over the limit, no marker", "x\n" * 3001, None, False, True),
        ("T3 marked, at its ceiling", big, None, False, False),
        ("T4 marked, over its ceiling", big + "x\n" * 10, None, False, True),
        ("T5 marked but back under the limit", "// awaken-allow(file-limit-ceiling): 3005\n" + small, None, False, True),
        ("T6 staged new file with a marker", big, None, True, True),
        ("T7 staged marker added to a file under the limit", big, small, True, True),
        ("T8 staged marker added to a legacy oversized file", big, "x\n" * 3004, True, False),
        ("T9 staged ceiling raised", big, "// awaken-allow(file-limit-ceiling): 3002\n" + "x\n" * 3000, True, True),
        ("T10 staged ceiling held", big, big, True, False),
    ]
    for name, text, previous, staged, warned in rows:
        if bool(test_file_warnings("tests/x.rs", text, previous, staged)) != warned:
            failures.append(f"{name}: expected {'warning' if warned else 'no finding'}")
    # A staged rename is judged against the committed file it moves (R1); a
    # copy, add or modify keeps its own path (R2); malformed status fails (R3).
    if rename_sources(["R100", "a/old.rs", "b/new.rs", "R087", "c.rs", "d.rs"]) != {"b/new.rs": "a/old.rs", "d.rs": "c.rs"}:
        failures.append("R1 a staged rename must resolve to its committed path")
    if rename_sources([]) != {}:
        failures.append("R2 no rename keeps every path as itself")
    for malformed in (["R100", "a.rs"], ["M", "a.rs", "b.rs"]):
        try:
            rename_sources(malformed)
        except ValueError:
            continue
        failures.append(f"R3 malformed rename status {malformed} must fail closed")
    # Cause/effect table for production Rust sources. Causes: functional lines vs
    # 800 / 1500, comments and cfg(test) code (never functional), a legacy ceiling
    # marker, inline tests vs 3000, physical lines vs 4000. Effects: no finding
    # or warning; line-only findings never enter the strict error channel.
    def body(functional: int, comments: int = 0, inline_tests: int = 0, marker: int | None = None) -> str:
        head = f"// awaken-allow(file-limit-ceiling): {marker}\n" if marker is not None else ""
        code = "".join(f"pub fn f{i}() {{}}\n" for i in range(functional))
        notes = "// note\n" * comments
        tests = ("#[cfg(test)]\nmod tests {\n" + "    fn t() {}\n" * max(inline_tests - 2, 0) + "}\n"
                 if inline_tests else "")
        return head + notes + code + tests

    rust_rows = [
        ("R1 under the warning line", body(799), False, False),
        ("R2 at the warning line warns", body(800), True, False),
        ("R3 above the functional reference without a marker", body(1501), True, False),
        ("R4 comments are not functional", body(700, comments=3000), False, False),
        ("R5 cfg(test) code is not functional", body(700, inline_tests=2000), False, False),
        ("R6 inline tests over the test limit", body(10, inline_tests=3002), True, False),
        ("R7 physical backstop", body(10, comments=4100), True, False),
        ("R8 legacy file at its ceiling", body(1600, marker=1600), False, False),
        ("R9 legacy file over its ceiling", body(1601, marker=1600), True, False),
    ]
    for name, text, warned, rejected in rust_rows:
        warns, errs = rust_source_results("crates/x/src/lib.rs", text, None, False)
        if bool(warns) != warned or bool(errs) != rejected:
            failures.append(f"{name}: expected warn={warned} reject={rejected}, got {warns} {errs}")
    # Cause/effect table for the rustfmt reflow. Causes: staged or whole-tree
    # scan, Rust or not, what the formatter returns. Effect: the measured text.
    # | W1 | staged | Rust     | reflows past the reference | formatted text -> warn   |
    # | W2 | staged | Rust     | cannot format (None)       | staged bytes             |
    # | W3 | staged | not Rust | (never called)             | staged bytes             |
    # | W4 | tree   | Rust     | (never called)             | bytes on disk            |
    # | W5 | real rustfmt, repository config                 | one line becomes several |
    def refused(rel: str, text: str) -> str:
        raise AssertionError(f"formatter called for {rel}")

    reflowed = as_committed({"crates/x/src/lib.rs": "pub fn f() {}\n"}, lambda rel, text: body(1501))
    warns, errs = rust_source_results("crates/x/src/lib.rs", reflowed["crates/x/src/lib.rs"], None, False)
    if not warns or errs:
        failures.append("W1 rustfmt reflow remains measured and warns without a line-only rejection")
    if as_committed({"crates/x/src/lib.rs": "fn f( {\n"}, lambda rel, text: None) != {"crates/x/src/lib.rs": "fn f( {\n"}:
        failures.append("W2 a file rustfmt cannot format keeps its staged bytes")
    if as_committed({"docs/x.md": "# x\n"}, refused) != {"docs/x.md": "# x\n"}:
        failures.append("W3 only Rust sources are formatted")
    import tempfile
    from unittest.mock import patch
    with tempfile.TemporaryDirectory(prefix="awaken-file-limit-tree-") as temporary:
        fixture = Path(temporary)
        (fixture / "fixture.rs").write_text("fn item() {}\n")
        with patch.dict(find_results.__globals__, REPO_ROOT=fixture):
            try:
                find_results(["fixture.rs"], staged=False, formatter=refused)
            except AssertionError:
                failures.append("W4 the whole-tree scan measures disk bytes without formatting")
    if shutil.which("rustfmt"):
        formatted = rustfmt_output("crates/x/src/lib.rs", "pub fn f() { let a = 1; let b = 2; }\n")
        if formatted is None or count_lines(formatted) < 4:
            failures.append(f"W5 real rustfmt must reflow a one-line body, got {formatted!r}")
    else:
        print("file-limit self-test: W5 not run -- rustfmt is not on PATH", file=sys.stderr)
    _selftest_cli_warnings()
    if failures:
        print("Self-test FAILED:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("OK - file-limit self-test passed.")
    return 0


def _selftest_source_ceilings() -> None:
    """SX3/SX4 decision table for a single historical/current decoder.

    Exact rule + number + first 5-after-opening-header -> original inline bound.
    Sixth line/bad number/old current syntax -> unmarked; stale/current>bound
    -> original warning. Current new syntax x HEAD old/new/unmarked/absent
    retains the original held N or genuinely-unmarked measure+1/new-file law.
    Cross held-1/equal/+1: the cutover must never substitute measure(HEAD)+1
    for an old held record. Actual main/Git fixtures test the complete staged
    read and original rustfmt projection; no second comparison is implemented.
    """
    import contextlib
    import io
    import tempfile
    from unittest.mock import patch

    current_prefix = "awaken-allow(file-limit-ceiling):"
    legacy_prefix = "awaken-" + "file-limit-ceiling:"
    body = "// existing test note\n" * 3000
    old = f"// {legacy_prefix} 3005 (legacy test file; owner=held; expires=opaque)\n" + body
    new = old.replace(legacy_prefix, current_prefix)
    assert declared_ceiling(old) is None and declared_ceiling(old, historical=True) == 3005, "SX4 historical-only old schema"
    assert declared_ceiling(new) == declared_ceiling(new, historical=True) == 3005, "SX4 one new decoder"
    assert declared_ceiling("\n" * 4 + new) == 3005, "SX3 fifth line"
    assert declared_ceiling("\n" * 5 + new) is None, "SX3 sixth line"
    anchors = "// @formal evidence held.rule\n" * 8
    assert declared_ceiling(anchors + "\n" * 4 + new) == 3005, "SX3 opening formal header excluded"
    assert declared_ceiling("// code note\n" + anchors + new) is None, "SX3 later anchors count"
    for payload, expected in (("3005", 3005), ("3005 owner=unchanged; expires=unchanged; foreign=opaque", 3005),
                              ("", None), ("N", None), ("-1", None)):
        assert declared_ceiling(f"// {current_prefix} {payload}\n") == expected, "SX3 original numeric payload"
    assert test_file_warnings("tests/probe.rs", old, old, True), "SX4 old current warns without providing an allowance"
    for previous in (old, new):
        for bound in (3004, 3005, 3006):
            candidate = new.replace("3005", str(bound), 1)
            assert bool(test_file_warnings("tests/probe.rs", candidate, previous, True)) == (bound > 3005), "SX4 held decrease/equal/growth across schemas"
    assert test_file_warnings("tests/probe.rs", new, None, True), "SX4 new oversized file warns"
    assert test_file_warnings("tests/probe.rs", new, "// short\n", True), "SX4 unmarked historical file under limit warns"
    assert not test_file_warnings("tests/probe.rs", new, "// note\n" * 3004, True), "SX4 genuinely unmarked legacy bound stays measure+1"
    # SX7 cause/effect: the production caller already measured the complete
    # tuple; reuse it. A marked HEAD needs no count, an unmarked HEAD exactly
    # one. Actual rust_measures instrumentation distinguishes parser reuse
    # from a stub scalar that could conceal a second current scan.
    real_measure = rust_measures
    production = "// awaken-allow(file-limit-ceiling): 1503\n" + "pub fn item() {}\n" * 1501
    unmarked = "pub fn item() {}\n" * 1502
    for previous, expected in ((production, [production]), (unmarked, [production, unmarked])):
        measured_sources = []
        def observed(text):
            measured_sources.append(text)
            return real_measure(text)
        with patch.dict(rust_source_results.__globals__, rust_measures=observed):
            assert not rust_source_results("crates/fixture/src/lib.rs", production, previous, True)[1], "SX7 original policy result"
        assert measured_sources == expected, "SX7 each required source projection/count only once"

    environment = formal_sources.detached_git_environment()
    with tempfile.TemporaryDirectory(prefix="awaken-ceiling-cutover-") as temporary:
        root = Path(temporary)
        (root / "tests").mkdir()
        source = root / "tests/probe.rs"
        def git(*arguments):
            return subprocess.check_output(["git", *arguments], cwd=root, env=environment)
        for previous in (old, new):
            source.write_text(previous)
            if not (root / ".git").exists():
                git("init", "--quiet")
            git("add", ".")
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                "-c", "core.hooksPath=/dev/null", "commit", "--allow-empty", "--quiet", "-m", "fixture")
            for bound in (3004, 3005, 3006):
                source.write_text(new.replace("3005", str(bound), 1))
                git("add", ".")
                before = source.read_bytes(), git("show", ":tests/probe.rs")
                output, errors = io.StringIO(), io.StringIO()
                with patch.dict(main.__globals__, REPO_ROOT=root), patch.dict(os.environ, environment, clear=True), \
                        contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                    status = main(["--staged"])
                assert status == 0, "SX4 actual staged main across old/new HEAD is advisory"
                if bound > 3005:
                    assert "from 3005" in errors.getvalue(), "SX4 diagnostics use original held N, not measured 3002"
                assert before == (source.read_bytes(), git("show", ":tests/probe.rs")), "SX4 diagnostics never rewrite marker/source/index bytes"


def _selftest_cli_warnings() -> None:
    """Actual CLI exit/output and immutable-source checks in a detached Git fixture.

    C1 production functional/inline/physical, test, other-code, docs and config
    references are exceeded; C2 tree/staged/own partial index selects the bytes;
    C3 an actual rename raises a marker against its original HEAD path.
    W1-3 => measured warning + exit0 + unchanged source/index bytes. C4 malformed
    rename framing, corrupt index or invalid argv => original nonzero failure;
    W4-6 never become a warning-only success. Thresholds and held marker values
    remain unchanged, and no fixture reads or initializes the invoking worktree.
    """
    import tempfile

    environment = formal_sources.detached_git_environment()
    script_directory = Path(__file__).resolve().parent
    bootstrap = (
        "import sys; from pathlib import Path; "
        "sys.path.insert(0, sys.argv[1]); import check_file_limits as owner; "
        "owner.REPO_ROOT = Path(sys.argv[2]); "
        "mode = sys.argv[3]; "
        "owner.staged_rename_sources = ("
        "lambda: owner.rename_sources(['R100', 'old.rs'])) "
        "if mode == 'malformed-rename' else owner.staged_rename_sources; "
        "sys.exit(owner.main(sys.argv[4:]))"
    )
    with tempfile.TemporaryDirectory(prefix="awaken-file-limit-cli-") as temporary:
        root = Path(temporary)
        for name in ("src", "tests", "docs"):
            (root / name).mkdir()
        (root / "Cargo.toml").write_text(
            '[package]\nname = "file-limit-fixture"\nversion = "0.0.0"\nedition = "2021"\n'
        )
        (root / "src/lib.rs").write_text("pub fn initial() {}\n")
        (root / "source.py").write_text("# short\n")
        (root / "docs/probe.md").write_text("# short\n")
        (root / "settings.json").write_text("{}\n")
        old = ("// awaken-" + "file-limit-ceiling: 3005\n") + "// note\n" * 3000
        (root / "tests/old.rs").write_text(old)

        def git(*arguments):
            return subprocess.check_output(["git", *arguments], cwd=root, env=environment)

        git("init", "--quiet")
        git("config", "core.autocrlf", "false")
        git("add", ".")
        git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "fixture")
        partial = root / ".git/clean-index"
        subprocess.check_output(["git", "read-tree", "HEAD"], cwd=root,
                                env={**environment, "GIT_INDEX_FILE": str(partial)})
        production = "".join(f"pub fn item{i}() {{}}\n" for i in range(1501))
        production += "// note\n" * 4100
        production += "#[cfg(test)]\nmod tests {\n"
        production += "".join(f"    fn test{i}() {{}}\n" for i in range(3000)) + "}\n"
        (root / "src/lib.rs").write_text(production)
        (root / "source.py").write_text("# note\n" * 2000)
        # Rename detection uses content, not the earlier `git mv` command. Keep
        # this other added file distinct from the historical rename source.
        (root / "tests/oversized.rs").write_text("// oversized test note\n" * 3001)
        (root / "docs/probe.md").write_text("# note\n" * 1201)
        (root / "settings.json").write_text("\n" * 1201)

        def invoke(arguments, *, mode="normal", hook=None):
            selected_environment = environment if hook is None else hook
            selected_index = Path(selected_environment.get("GIT_INDEX_FILE", root / ".git/index"))
            before = {path.relative_to(root): path.read_bytes()
                      for path in root.rglob("*") if path.is_file() and ".git" not in path.parts}
            index_before = selected_index.read_bytes()
            result = subprocess.run(
                [sys.executable, "-B", "-c", bootstrap, str(script_directory), str(root), mode, *arguments],
                cwd=root, env=selected_environment, capture_output=True, text=True,
            )
            after = {path.relative_to(root): path.read_bytes()
                     for path in root.rglob("*") if path.is_file() and ".git" not in path.parts}
            assert before == after and index_before == selected_index.read_bytes(), "W1-6 CLI leaves source and selected index byte-equal"
            return result

        git("add", ".")
        for arguments in ([], ["--staged"]):
            result = invoke(arguments)
            assert result.returncode == 0, f"W1 line-only CLI success: {result.stderr}"
            for diagnostic in ("File length warnings", "functional lines", "inline test lines", "backstop",
                               "tests/oversized.rs", "source.py", "docs/probe.md", "settings.json"):
                assert diagnostic in result.stderr, f"W1 original measurement missing: {diagnostic}"
            assert "1201" in result.stderr and "2000" in result.stderr, "W1 original numeric references remain visible"
        own = {**environment, "GIT_DIR": str(root / ".git"), "GIT_WORK_TREE": str(root),
               "GIT_INDEX_FILE": str(partial)}
        clean = invoke(["--staged"], hook=own)
        assert clean.returncode == 0 and "1201" not in clean.stderr, "W2 clean selected index cannot read oversized working bytes"

        git("mv", "tests/old.rs", "tests/renamed.rs")
        renamed = root / "tests/renamed.rs"
        renamed.write_text(old.replace("awaken-file-limit-ceiling: 3005",
                                       "awaken-allow(file-limit-ceiling): 3006", 1))
        git("add", ".")
        moved = invoke(["--staged"])
        assert moved.returncode == 0 and "tests/renamed.rs" in moved.stderr and "from 3005" in moved.stderr, f"W3 real rename retains original HEAD ceiling as warning: {moved.stderr}"
        malformed = invoke(["--staged"], mode="malformed-rename")
        assert malformed.returncode != 0 and "rename status fields" in malformed.stderr, "W4 malformed rename still fails"
        broken = root / ".git/broken-index"
        broken.write_bytes(b"not an index")
        invalid_hook = {**own, "GIT_INDEX_FILE": str(broken)}
        unreadable = invoke(["--staged"], hook=invalid_hook)
        assert unreadable.returncode != 0 and "index" in unreadable.stderr, "W5 actual Git input failure still fails"
        argv = invoke(["--unknown"], hook=invalid_hook)
        assert argv.returncode == 2 and "unrecognized arguments" in argv.stderr, "W6 argv parsing still precedes source IO"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("--staged", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()

    files = (formal_sources.git_paths(REPO_ROOT, "diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR")
             if args.staged else formal_sources.tracked_files(REPO_ROOT, allow_nonrepository=True))
    if args.staged and not files:
        print("OK - no staged files to size-check.")
        return 0

    warnings, errors, _has_code_violation = find_results(files, args.staged)
    if warnings:
        print("File length warnings:", file=sys.stderr)
        for warning in warnings:
            print(f"  {warning}", file=sys.stderr)
        print(
            "\nLine counts and recorded ceilings are advisory. Review ownership "
            "and responsibility boundaries; keep tests with the behaviour they verify.",
            file=sys.stderr,
        )
    if errors:
        print("File scan failed:", file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
        return 1
    print("OK - file length diagnostics completed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
