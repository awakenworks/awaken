#!/usr/bin/env python3
"""Fail when changed production Rust lines lack served-process API E2E coverage."""

from __future__ import annotations

import argparse
import collections
import pathlib
import re
import subprocess
import sys
import tomllib


ROOT = pathlib.Path(__file__).resolve().parents[2]
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")
TEST_MODULE = re.compile(r"^\s*#\s*\[\s*cfg\s*\([^]]*\btest\b[^]]*\)\s*\]")


def run(*args: str) -> str:
    return subprocess.run(
        args,
        cwd=ROOT,
        check=True,
        encoding="utf-8",
        stdout=subprocess.PIPE,
    ).stdout


def diff_base(base: str) -> str:
    """Resolve the branch point while retaining uncommitted production edits."""

    return run("git", "merge-base", base, "HEAD").strip()


def test_only_lines(relative: str) -> set[int]:
    """Return source lines owned by Rust's compile-time test surface.

    Separate ``src/tests.rs`` modules are wholly test-only. Inline test modules
    conventionally sit at the end of their production module; once their
    ``#[cfg(test)]`` attribute begins, no later item is part of a shipped binary.
    Keeping these lines out of the denominator prevents adding tests from making
    production coverage regress by construction.
    """

    path = pathlib.PurePosixPath(relative)
    source = (ROOT / relative).read_text(encoding="utf-8").splitlines()
    if path.name == "tests.rs" or "tests" in path.parts:
        return set(range(1, len(source) + 1))
    for number, line in enumerate(source, 1):
        if TEST_MODULE.match(line):
            following = source[number : min(number + 5, len(source))]
            item = next(
                (
                    candidate
                    for candidate in following
                    if candidate.strip()
                    and not candidate.lstrip().startswith(("#", "//"))
                ),
                "",
            )
            if re.match(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{", item):
                return set(range(number, len(source) + 1))
    return set()


def changed_lines(base: str, ignore: re.Pattern[str] | None) -> dict[str, set[int]]:
    baseline = diff_base(base)
    names = run("git", "diff", "--name-only", "--diff-filter=ACMR", baseline, "--")
    result: dict[str, set[int]] = {}
    for relative in names.splitlines():
        if not (
            relative.startswith("crates/")
            and "/src/" in relative
            and relative.endswith(".rs")
        ):
            continue
        if ignore is not None and ignore.search(relative):
            continue
        lines: set[int] = set()
        diff = run("git", "diff", "--unified=0", baseline, "--", relative)
        for line in diff.splitlines():
            match = HUNK.match(line)
            if not match:
                continue
            start = int(match.group(1))
            count = int(match.group(2) or "1")
            lines.update(range(start, start + count))
        if lines:
            production = lines - test_only_lines(relative)
            if production:
                result[relative] = production
    return result


def lcov_lines(ignore_filename_regex: str | None) -> dict[str, dict[int, int]]:
    command = ["cargo", "llvm-cov", "report", "--lcov"]
    if ignore_filename_regex:
        command.extend(["--ignore-filename-regex", ignore_filename_regex])
    lcov = run(*command)
    result: dict[str, dict[int, int]] = {}
    current: str | None = None
    for line in lcov.splitlines():
        if line.startswith("SF:"):
            source = pathlib.Path(line[3:])
            try:
                current = source.resolve().relative_to(ROOT).as_posix()
            except ValueError:
                current = None
        elif current is not None and line.startswith("DA:"):
            number, count, *_ = line[3:].split(",")
            bucket = result.setdefault(current, {})
            line_number = int(number)
            bucket[line_number] = max(bucket.get(line_number, 0), int(count))
    return result


def unreachable_lines(manifest: str | None) -> tuple[dict[str, set[int]], list[dict[str, object]]]:
    """Load audited lines that no served-process API can deterministically select.

    A waiver never turns a hit into an exclusion: callers apply it only to changed,
    executable lines whose counter is zero.  This keeps newly covered lines in the
    numerator and makes the manifest an explicit review queue rather than a way to
    shrink already-observed production behavior.
    """

    if manifest is None:
        return {}, []
    manifest_path = ROOT / manifest
    document = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    if document.get("version") != 1:
        raise ValueError(f"{manifest}: expected version = 1")
    by_path: dict[str, set[int]] = collections.defaultdict(set)
    entries = document.get("waiver", [])
    if not isinstance(entries, list) or not entries:
        raise ValueError(f"{manifest}: expected at least one [[waiver]]")
    normalized: list[dict[str, object]] = []
    for index, entry in enumerate(entries, 1):
        if not isinstance(entry, dict):
            raise ValueError(f"{manifest}: waiver {index} is not a table")
        path = entry.get("path")
        ranges = entry.get("ranges")
        reason = entry.get("reason")
        evidence = entry.get("evidence")
        if not isinstance(path, str) or not path.startswith("crates/") or not path.endswith(".rs"):
            raise ValueError(f"{manifest}: waiver {index} has an invalid Rust path")
        source = ROOT / path
        if not source.is_file():
            raise ValueError(f"{manifest}: waiver {index} path does not exist: {path}")
        if not isinstance(reason, str) or len(reason.strip()) < 20:
            raise ValueError(f"{manifest}: waiver {index} needs a specific reason")
        if not isinstance(evidence, str) or len(evidence.strip()) < 10:
            raise ValueError(f"{manifest}: waiver {index} needs test/formal evidence")
        if not isinstance(ranges, list) or not ranges:
            raise ValueError(f"{manifest}: waiver {index} needs line ranges")
        line_count = len(source.read_text(encoding="utf-8").splitlines())
        selected: set[int] = set()
        for value in ranges:
            if not isinstance(value, str) or not re.fullmatch(r"\d+(?:-\d+)?", value):
                raise ValueError(f"{manifest}: invalid range {value!r} for {path}")
            start_text, _, end_text = value.partition("-")
            start = int(start_text)
            end = int(end_text or start_text)
            if start < 1 or end < start or end > line_count:
                raise ValueError(
                    f"{manifest}: range {value} exceeds {path} (1-{line_count})"
                )
            selected.update(range(start, end + 1))
        overlap = by_path[path] & selected
        if overlap:
            raise ValueError(
                f"{manifest}: overlapping waiver lines for {path}: {sorted(overlap)[:5]}"
            )
        by_path[path].update(selected)
        normalized.append(
            {"path": path, "lines": selected, "reason": reason, "evidence": evidence}
        )
    return dict(by_path), normalized


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="origin/1.0.0-dev")
    parser.add_argument("--minimum", type=float, default=0.95)
    parser.add_argument(
        "--ignore-filename-regex",
        help="apply the production reachability exclusions used by the coverage report",
    )
    parser.add_argument("--show-missing", type=int, default=80)
    parser.add_argument("--show-files", type=int, default=30)
    parser.add_argument(
        "--unreachable-manifest",
        help="audited changed lines that cannot be selected through a served API",
    )
    parser.add_argument(
        "--maximum-unreachable-fraction",
        type=float,
        default=0.15,
        help="fail if audited non-API lines exceed this share of changed executable lines",
    )
    parser.add_argument(
        "--label",
        default="changed API E2E line coverage",
        help="human-readable authority name printed in the report and failures",
    )
    args = parser.parse_args()
    if not 0.0 < args.minimum < 1.0:
        parser.error("--minimum must be between zero and one")
    if not 0.0 <= args.maximum_unreachable_fraction < 1.0:
        parser.error("--maximum-unreachable-fraction must be between zero and one")

    try:
        ignore = (
            re.compile(args.ignore_filename_regex)
            if args.ignore_filename_regex is not None
            else None
        )
    except re.error as error:
        parser.error(f"invalid --ignore-filename-regex: {error}")
    changed = changed_lines(args.base, ignore)
    coverage = lcov_lines(args.ignore_filename_regex)
    try:
        unreachable, waiver_entries = unreachable_lines(args.unreachable_manifest)
    except (OSError, ValueError, tomllib.TOMLDecodeError) as error:
        parser.error(str(error))
    executable: list[tuple[str, int, int]] = []
    for relative, lines in changed.items():
        measured = coverage.get(relative, {})
        executable.extend(
            (relative, number, measured[number])
            for number in sorted(lines & measured.keys())
        )
    if not executable:
        raise SystemExit(f"{args.label}: no changed executable Rust lines found")

    covered = [(path, line, count) for path, line, count in executable if count > 0]
    waived = [
        (path, line)
        for path, line, count in executable
        if count == 0 and line in unreachable.get(path, set())
    ]
    missing = [
        (path, line)
        for path, line, count in executable
        if count == 0 and line not in unreachable.get(path, set())
    ]
    reachable_total = len(covered) + len(missing)
    ratio = len(covered) / reachable_total
    print(
        f"{args.label}: {len(covered)}/{reachable_total} = {ratio:.2%} "
        f"(required > {args.minimum:.0%}, base {args.base})"
    )
    if waived:
        waived_fraction = len(waived) / len(executable)
        print(
            f"  audited non-API-reachable changed lines: {len(waived)} "
            f"({waived_fraction:.2%}; raw executable total: {len(executable)})"
        )
        if waived_fraction > args.maximum_unreachable_fraction:
            raise SystemExit(
                "audited non-API-reachable share exceeds its ceiling: "
                f"{waived_fraction:.2%} > {args.maximum_unreachable_fraction:.2%}"
            )
    stale = []
    for entry in waiver_entries:
        path = str(entry["path"])
        selected = entry["lines"]
        assert isinstance(selected, set)
        if not any(candidate_path == path and line in selected for candidate_path, line in waived):
            stale.append(path)
    if stale:
        raise SystemExit(
            "unreachable waiver no longer matches an uncovered changed executable line: "
            + ", ".join(stale)
        )
    by_file: dict[str, list[int]] = collections.defaultdict(lambda: [0, 0])
    for path, line, count in executable:
        if count == 0 and line in unreachable.get(path, set()):
            continue
        by_file[path][1] += 1
        if count > 0:
            by_file[path][0] += 1
    ranked = sorted(
        by_file.items(),
        key=lambda item: (item[1][1] - item[1][0], item[1][1]),
        reverse=True,
    )
    for path, (file_covered, file_total) in ranked[: args.show_files]:
        print(
            f"  file {file_covered}/{file_total} = {file_covered / file_total:.2%} {path}"
        )
    for path, line in missing[: args.show_missing]:
        print(f"  uncovered {path}:{line}")
    if len(missing) > args.show_missing:
        print(f"  ... and {len(missing) - args.show_missing} more")
    if ratio <= args.minimum:
        raise SystemExit(f"{args.label} is below the strict threshold")


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        print(f"changed line coverage command failed: {error}", file=sys.stderr)
        raise SystemExit(error.returncode) from error
