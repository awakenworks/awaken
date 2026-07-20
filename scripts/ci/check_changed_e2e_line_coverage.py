#!/usr/bin/env python3
"""Fail when executable Rust lines changed from a Git base lack E2E coverage."""

from __future__ import annotations

import argparse
import collections
import pathlib
import re
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[2]
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")


def run(*args: str) -> str:
    return subprocess.run(
        args, cwd=ROOT, check=True, text=True, stdout=subprocess.PIPE
    ).stdout


def changed_lines(base: str) -> dict[str, set[int]]:
    names = run("git", "diff", "--name-only", "--diff-filter=AMR", base, "--")
    result: dict[str, set[int]] = {}
    for relative in names.splitlines():
        if not (
            relative.startswith("crates/")
            and "/src/" in relative
            and relative.endswith(".rs")
        ):
            continue
        lines: set[int] = set()
        diff = run("git", "diff", "--unified=0", base, "--", relative)
        for line in diff.splitlines():
            match = HUNK.match(line)
            if not match:
                continue
            start = int(match.group(1))
            count = int(match.group(2) or "1")
            lines.update(range(start, start + count))
        if lines:
            result[relative] = lines
    return result


def lcov_lines() -> dict[str, dict[int, int]]:
    lcov = run("cargo", "llvm-cov", "report", "--lcov")
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


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="origin/1.0.0-dev")
    parser.add_argument("--minimum", type=float, default=0.95)
    parser.add_argument("--show-missing", type=int, default=80)
    parser.add_argument("--show-files", type=int, default=30)
    args = parser.parse_args()
    if not 0.0 < args.minimum < 1.0:
        parser.error("--minimum must be between zero and one")

    changed = changed_lines(args.base)
    coverage = lcov_lines()
    executable: list[tuple[str, int, int]] = []
    for relative, lines in changed.items():
        measured = coverage.get(relative, {})
        executable.extend(
            (relative, number, measured[number])
            for number in sorted(lines & measured.keys())
        )
    if not executable:
        raise SystemExit("changed E2E line coverage: no changed executable Rust lines found")

    covered = [(path, line, count) for path, line, count in executable if count > 0]
    missing = [(path, line) for path, line, count in executable if count == 0]
    ratio = len(covered) / len(executable)
    print(
        f"changed E2E line coverage: {len(covered)}/{len(executable)} = {ratio:.2%} "
        f"(required > {args.minimum:.0%}, base {args.base})"
    )
    by_file: dict[str, list[int]] = collections.defaultdict(lambda: [0, 0])
    for path, _line, count in executable:
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
        raise SystemExit("changed E2E line coverage is below the strict threshold")


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        print(f"changed E2E line coverage command failed: {error}", file=sys.stderr)
        raise SystemExit(error.returncode) from error
