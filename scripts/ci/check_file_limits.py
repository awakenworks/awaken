#!/usr/bin/env python3
"""Enforce file length limits for source, config, and design files.

Rust source files are measured by what they are, not by how long they look:

* functional lines -- non-blank production lines, comments and `cfg(test)` code
  excluded -- warn at RUST_FUNCTIONAL_WARNING_LIMIT and stop at
  RUST_FUNCTIONAL_ERROR_LIMIT; a file already over the stop line when the rule
  arrived carries an `awaken-allow(file-limit-ceiling): N` marker (functional lines) that
  may only fall;
* inline test lines (the file's `cfg(test)` code) stop at TEST_ERROR_LIMIT, the
  room a separate test file gets, so tests can stay beside the code they verify;
* physical lines stop at RUST_PHYSICAL_BACKSTOP so a file stays navigable.

A Rust file is a test file when its path says so or when its crate does not
compile it into production (a `#[cfg(test)] mod x;` file); test files keep the
physical test limit. Other languages keep the physical CODE_* limits.

* `file-limit-ceiling`: the numeric allowance in the first 5 lines after the
  opening formal header may only fall against HEAD. This marker does not
  authorize a new oversized file or a stale allowance below the hard limit.
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
# Test code had no limit at all, so single files grew to 27k lines and became
# the most-rebased files in the tree (runtime-host host/tests.rs was touched by
# 46 of 500 commits). Tests get more room than production, not unlimited room.
TEST_ERROR_LIMIT = 3000
RUST_FUNCTIONAL_WARNING_LIMIT = 800
RUST_FUNCTIONAL_ERROR_LIMIT = 1500
RUST_PHYSICAL_BACKSTOP = 4000
# A test file that was already over TEST_ERROR_LIMIT when the limit arrived
# carries its ceiling in its own first lines, as `awaken-allow(file-limit-ceiling): N`.
# The ceiling lives with the file, not in a path list here, so a split or a
# rename carries it (or drops it) in the same diff. At commit time the ceiling
# may only fall: it may not exceed the committed one, and a file that had none
# may take one only if it was already over the limit, which after the first
# commit is never. Once the file is back under the limit the marker is stale.
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
            return "error"
        warning_limit = warning_limit_for(rel)
        if warning_limit is not None and lines >= warning_limit:
            return "warning"
        return None
    return "error" if lines > limit else None


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


def test_file_errors(rel: str, text: str, previous: str | None, staged: bool) -> list[str]:
    """The test-file limit with its in-file ceiling, judged against HEAD when staged."""
    return ceiling_errors(
        rel, text, previous, staged, count_lines, TEST_ERROR_LIMIT, "lines", "test limit",
        "split the file by the behaviour its tests verify",
        "an oversized test file may shrink but never grow -- add new tests in a new file",
    )


def ceiling_errors(rel: str, text: str, previous: str | None, staged: bool, measure, limit: int,
                   unit: str, label: str, split_advice: str, grow_advice: str,
                   *, measured: int | None = None) -> list[str]:
    """A hard limit that a legacy file's in-file numeric allowance may exceed, only downward."""

    lines = measure(text) if measured is None else measured
    ceiling = declared_ceiling(text)
    errors: list[str] = []
    if ceiling is None:
        if lines > limit:
            errors.append(f"{rel}: {lines} {unit} > {label} {limit}; {split_advice}")
        return errors
    if lines <= limit:
        errors.append(
            f"{rel}: {lines} {unit} is within the {label} {limit}; remove its "
            "awaken-allow(file-limit-ceiling) marker so it cannot grow back"
        )
        return errors
    if lines > ceiling:
        errors.append(f"{rel}: {lines} {unit} > its awaken-allow(file-limit-ceiling) {ceiling}; {grow_advice}")
    if staged:
        if previous is None:
            errors.append(
                f"{rel}: a new file may not declare awaken-allow(file-limit-ceiling); keep it under {limit} {unit}"
            )
        else:
            before = declared_ceiling(previous, historical=True)
            previous_lines = measure(previous) if before is None else None
            bound = before if before is not None else previous_lines + 1
            if before is None and previous_lines <= limit:
                errors.append(
                    f"{rel}: declares awaken-allow(file-limit-ceiling) {ceiling} but was within the {label} "
                    f"{limit}; a ceiling may not be introduced to admit growth"
                )
            elif ceiling > bound:
                errors.append(
                    f"{rel}: raises awaken-allow(file-limit-ceiling) to {ceiling} from {bound}; the ceiling only falls"
                )
    return errors


def rust_source_results(rel: str, text: str, previous: str | None, staged: bool) -> tuple[list[str], list[str]]:
    """Warnings and errors for one production Rust source file."""
    functional, inline_tests, physical = rust_measures(text)
    errors = ceiling_errors(
        rel, text, previous, staged, functional_lines, RUST_FUNCTIONAL_ERROR_LIMIT, "functional lines",
        "functional limit", "split the module by responsibility",
        "a file over the functional limit may shrink but never grow -- put new behaviour in a new module",
        measured=functional,
    )
    if inline_tests > TEST_ERROR_LIMIT:
        errors.append(f"{rel}: {inline_tests} inline test lines > test limit {TEST_ERROR_LIMIT}; "
                      "split the tests by the behaviour they verify")
    if physical > RUST_PHYSICAL_BACKSTOP:
        errors.append(f"{rel}: {physical} lines > backstop {RUST_PHYSICAL_BACKSTOP}; split the file")
    warnings = []
    if not errors and RUST_FUNCTIONAL_WARNING_LIMIT <= functional < RUST_FUNCTIONAL_ERROR_LIMIT:
        warnings.append(
            f"{rel}: {functional} functional lines >= warning threshold {RUST_FUNCTIONAL_WARNING_LIMIT}"
        )
    return warnings, errors


def find_results(files: list[str], staged: bool,
                 formatter=rustfmt_output) -> tuple[list[str], list[str], bool]:
    errors: list[str] = []
    warnings: list[str] = []
    has_code_violation = False
    texts = {rel: text for rel in sorted(files)
             if limit_for(rel) is not None and (text := formal_sources.file_text(REPO_ROOT, rel, staged)) is not None}
    if staged:
        texts = as_committed(texts, formatter)
    for rel, text in texts.items():
        limit = limit_for(rel)
        if is_rust_source(rel):
            previous = formal_sources.committed_text(REPO_ROOT, rel) if staged else None
            found_warnings, found = rust_source_results(rel, text, previous, staged)
            warnings.extend(found_warnings)
            errors.extend(found)
            has_code_violation = has_code_violation or bool(found)
            continue
        if is_test_code_file(rel) or Path(rel).suffix == ".rs":
            previous = formal_sources.committed_text(REPO_ROOT, rel) if staged else None
            found = test_file_errors(rel, text, previous, staged)
            errors.extend(found)
            has_code_violation = has_code_violation or bool(found)
            continue
        lines = count_lines(text)
        classification = classify_line_count(rel, lines)
        if classification == "error":
            if is_code_file(rel):
                errors.append(f"{rel}: {lines} lines >= hard limit {limit}")
                has_code_violation = True
            else:
                errors.append(f"{rel}: {lines} lines > {limit}")
            continue
        if classification == "warning":
            warnings.append(f"{rel}: {lines} lines >= warning threshold {CODE_WARNING_LIMIT}")
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
        ("src/lib.rs", CODE_ERROR_LIMIT, "error"),
        ("docs/design/runtime.md", DOC_LIMIT, None),
        ("docs/design/runtime.md", DOC_LIMIT + 1, "error"),
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
    # under limit / unmarked over limit / marked). Effects: E1 accept, E2 reject.
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
    for name, text, previous, staged, rejected in rows:
        if bool(test_file_errors("tests/x.rs", text, previous, staged)) != rejected:
            failures.append(f"{name}: expected {'reject' if rejected else 'accept'}")
    # Cause/effect table for production Rust sources. Causes: functional lines vs
    # 800 / 1500, comments and cfg(test) code (never functional), a legacy ceiling
    # marker, inline tests vs 3000, physical lines vs 4000. Effects: accept, warn, reject.
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
        ("R3 at the stop line without a marker", body(1501), False, True),
        ("R4 comments are not functional", body(700, comments=3000), False, False),
        ("R5 cfg(test) code is not functional", body(700, inline_tests=2000), False, False),
        ("R6 inline tests over the test limit", body(10, inline_tests=3002), False, True),
        ("R7 physical backstop", body(10, comments=4100), False, True),
        ("R8 legacy file at its ceiling", body(1600, marker=1600), False, False),
        ("R9 legacy file over its ceiling", body(1601, marker=1600), False, True),
    ]
    for name, text, warned, rejected in rust_rows:
        warns, errs = rust_source_results("crates/x/src/lib.rs", text, None, False)
        if bool(warns) != warned or bool(errs) != rejected:
            failures.append(f"{name}: expected warn={warned} reject={rejected}, got {warns} {errs}")
    # Cause/effect table for the rustfmt reflow. Causes: staged or whole-tree
    # scan, Rust or not, what the formatter returns. Effect: the measured text.
    # | W1 | staged | Rust     | reflows past the stop line | formatted text -> reject |
    # | W2 | staged | Rust     | cannot format (None)       | staged bytes             |
    # | W3 | staged | not Rust | (never called)             | staged bytes             |
    # | W4 | tree   | Rust     | (never called)             | bytes on disk            |
    # | W5 | real rustfmt, repository config                 | one line becomes several |
    def refused(rel: str, text: str) -> str:
        raise AssertionError(f"formatter called for {rel}")

    reflowed = as_committed({"crates/x/src/lib.rs": "pub fn f() {}\n"}, lambda rel, text: body(1501))
    if not rust_source_results("crates/x/src/lib.rs", reflowed["crates/x/src/lib.rs"], None, False)[1]:
        failures.append("W1 a staged file rustfmt reflows past the stop line must be rejected")
    if as_committed({"crates/x/src/lib.rs": "fn f( {\n"}, lambda rel, text: None) != {"crates/x/src/lib.rs": "fn f( {\n"}:
        failures.append("W2 a file rustfmt cannot format keeps its staged bytes")
    if as_committed({"docs/x.md": "# x\n"}, refused) != {"docs/x.md": "# x\n"}:
        failures.append("W3 only Rust sources are formatted")
    on_disk = next((rel for rel in formal_sources.tracked_files(REPO_ROOT, allow_nonrepository=True)
                    if rel.endswith(".rs") and limit_for(rel)), None)
    try:
        find_results([on_disk] if on_disk else [], staged=False, formatter=refused)
    except AssertionError:
        failures.append("W4 the whole-tree scan measures committed files as they are")
    if shutil.which("rustfmt"):
        formatted = rustfmt_output("crates/x/src/lib.rs", "pub fn f() { let a = 1; let b = 2; }\n")
        if formatted is None or count_lines(formatted) < 4:
            failures.append(f"W5 real rustfmt must reflow a one-line body, got {formatted!r}")
    else:
        print("file-limit self-test: W5 not run -- rustfmt is not on PATH", file=sys.stderr)
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
    -> original rejection. Current new syntax x HEAD old/new/unmarked/absent
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
    assert test_file_errors("tests/probe.rs", old, old, True), "SX4 old current cannot provide an allowance"
    for previous in (old, new):
        for bound in (3004, 3005, 3006):
            candidate = new.replace("3005", str(bound), 1)
            assert bool(test_file_errors("tests/probe.rs", candidate, previous, True)) == (bound > 3005), "SX4 held decrease/equal/growth across schemas"
    assert test_file_errors("tests/probe.rs", new, None, True), "SX4 new oversized file refused"
    assert test_file_errors("tests/probe.rs", new, "// short\n", True), "SX4 unmarked historical file under limit"
    assert not test_file_errors("tests/probe.rs", new, "// note\n" * 3004, True), "SX4 genuinely unmarked legacy bound stays measure+1"
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
                output, errors = io.StringIO(), io.StringIO()
                with patch.dict(main.__globals__, REPO_ROOT=root), patch.dict(os.environ, environment, clear=True), \
                        contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                    status = main(["--staged"])
                assert status == int(bound > 3005), "SX4 actual staged main across old/new HEAD"
                if bound > 3005:
                    assert "from 3005" in errors.getvalue(), "SX4 diagnostics use original held N, not measured 3002"


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

    warnings, errors, has_code_violation = find_results(files, args.staged)
    if warnings:
        print("File length warnings:", file=sys.stderr)
        for warning in warnings:
            print(f"  {warning}", file=sys.stderr)
        print(
            "\nRust sources over 800 functional lines (other languages: 1000 lines) "
            "should be split before they reach the hard limit (1500 functional lines; "
            "2000 lines elsewhere). Do not respond by moving tests "
            "away, deleting tests, or weakening coverage; split production "
            "responsibilities and keep tests with the behavior they verify.",
            file=sys.stderr,
        )
    if errors:
        print("File length limits failed:", file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
        if has_code_violation:
            print(
                "\nFor backend/frontend source files, do not pass this gate by "
                "moving tests away, deleting tests, or weakening coverage. Split "
                "the production module/component by responsibility and keep tests "
                "with the behavior they verify.",
                file=sys.stderr,
            )
        print("\nSplit large docs by bounded design topic before committing.", file=sys.stderr)
        return 1
    print("OK - file length limits passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
