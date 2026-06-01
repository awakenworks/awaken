#!/usr/bin/env python3
"""Block source additions that would weaken Awaken guardrails.

The pre-commit path scans added lines only. This preserves legacy cleanup work
while preventing new silent fallback, hidden downgrade, or facade widening code.
Use `awaken-allow: A-GN` on the same line only for a deliberately reviewed
exception.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
ALLOW_RE = re.compile(r"awaken-allow:\s*A-G(?P<n>[0-9]+)")
HUNK_RE = re.compile(r"@@ -\d+(?:,\d+)? \+(?P<start>\d+)(?:,\d+)? @@")


@dataclass(frozen=True)
class AddedLine:
    path: str
    line_no: int
    text: str


@dataclass(frozen=True)
class Rule:
    code: str
    description: str
    pattern: re.Pattern[str]
    path_predicate: Callable[[str], bool]
    exclude_substrings: tuple[str, ...] = field(default_factory=tuple)


def _under(*prefixes: str) -> Callable[[str], bool]:
    normalized = tuple(prefix.rstrip("/") + "/" for prefix in prefixes)

    def check(path: str) -> bool:
        return any(path.startswith(prefix) for prefix in normalized)

    return check


def _exact(*paths: str) -> Callable[[str], bool]:
    allowed = set(paths)

    def check(path: str) -> bool:
        return path in allowed

    return check


def _or(*predicates: Callable[[str], bool]) -> Callable[[str], bool]:
    def check(path: str) -> bool:
        return any(predicate(path) for predicate in predicates)

    return check


RULES: tuple[Rule, ...] = (
    Rule(
        code="A-G2",
        description="Postgres run-row decode must not hide corrupt JSON/status data",
        pattern=re.compile(r"serde_json::from_value\([^\n]*\)\.ok\(\)|unwrap_or\(\s*RunStatus::Running\s*\)"),
        path_predicate=_exact("crates/awaken-stores/src/postgres/run.rs"),
    ),
    Rule(
        code="A-G5",
        description="pinned registry resolution must not downgrade to latest/live scope",
        pattern=re.compile(r"RegistryResolutionScope::Pinned.*LatestPublication|Pinned\([^)]*\).*LatestPublication|Pinned.*live_registry|live_registry.*Pinned"),
        path_predicate=_or(
            _under("crates/awaken-server/src"),
            _under("crates/awaken-runtime/src/registry/resolve"),
        ),
    ),
    Rule(
        code="A-G5",
        description="critical registry/durable-preview publication errors must not be converted to boolean success",
        pattern=re.compile(r"(publish_ephemeral_with_extra_agent|materialize|materialize_pinned_registry)[^\n]*\.(is_ok|ok)\(\)"),
        path_predicate=_under("crates/awaken-server/src"),
    ),
    Rule(
        code="A-G6",
        description="Replayable plans must be built through pinned snapshot provenance, not plain RegistrySetResolver::new",
        pattern=re.compile(r"ResolvedRunPlan::Replayable|Replayability::Replayable"),
        path_predicate=_under("crates/awaken-runtime/src/registry/resolve"),
        exclude_substrings=("/tests.rs", "/tests/"),
    ),
    Rule(
        code="A-G7",
        description="runtime event capture must not depend on dispatch-time panic checks",
        pattern=re.compile(r"expect\([^\n]*(runtime event capture|EventBuffer|staged commit coordinator)"),
        path_predicate=_under("crates/awaken-server/src/mailbox"),
    ),
    Rule(
        code="A-G10",
        description="agent prelude must not export server or store integration surfaces",
        pattern=re.compile(r"pub use .*(awaken_server|awaken_stores|crate::server|crate::stores|server_contract|ScopedThreadRunStore)"),
        path_predicate=_exact("crates/awaken/src/prelude.rs"),
    ),
)


def _staged_added_lines() -> list[AddedLine]:
    diff = subprocess.check_output(
        ["git", "diff", "--cached", "--unified=0", "--diff-filter=ACMR", "--", "*.rs"],
        cwd=REPO_ROOT,
        text=True,
    )
    lines: list[AddedLine] = []
    current_path: str | None = None
    new_line = 0
    for raw in diff.splitlines():
        if raw.startswith("+++ b/"):
            current_path = raw.removeprefix("+++ b/")
            continue
        if raw.startswith("@@ "):
            match = HUNK_RE.search(raw)
            new_line = int(match.group("start")) if match else 0
            continue
        if raw.startswith("+") and not raw.startswith("+++") and current_path:
            lines.append(AddedLine(current_path, new_line, raw[1:]))
            new_line += 1
            continue
        if not raw.startswith("-"):
            new_line += 1
    return lines


def _line_allowed(line: AddedLine, rule: Rule) -> bool:
    marker = ALLOW_RE.search(line.text)
    return bool(marker and f"A-G{marker.group('n')}" == rule.code)


def scan_added_lines(lines: list[AddedLine]) -> list[str]:
    violations: list[str] = []
    for line in lines:
        for rule in RULES:
            if not rule.path_predicate(line.path):
                continue
            decorated_path = f"/{line.path}"
            if any(excluded in decorated_path for excluded in rule.exclude_substrings):
                continue
            if rule.pattern.search(line.text) and not _line_allowed(line, rule):
                violations.append(
                    f"{line.path}:{line.line_no}: [{rule.code}] {rule.description}: {line.text.strip()}"
                )
    return violations


def self_test() -> int:
    lines = [
        AddedLine(
            "crates/awaken-stores/src/postgres/run.rs",
            12,
            "let waiting = serde_json::from_value(value).ok();",
        ),
        AddedLine(
            "crates/awaken-server/src/mailbox/runtime_event_capture.rs",
            8,
            'let buffer = event_buffer.expect("runtime event capture requires a per-run EventBuffer");',
        ),
        AddedLine(
            "crates/awaken/src/prelude.rs",
            4,
            "pub use awaken_stores::MemoryCommitCoordinator;",
        ),
        AddedLine(
            "crates/awaken/src/prelude.rs",
            5,
            "pub use awaken_stores::MemoryCommitCoordinator; // awaken-allow: A-G10",
        ),
    ]
    violations = scan_added_lines(lines)
    if len(violations) != 3:
        sys.stderr.write("self-test expected exactly three violations\n")
        sys.stderr.write("\n".join(violations) + "\n")
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--staged", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.staged:
        sys.stderr.write("use --staged so existing legacy lines do not block unrelated changes\n")
        return 2

    violations = scan_added_lines(_staged_added_lines())
    if violations:
        sys.stderr.write("ERROR: source guardrail violations found:\n")
        for violation in violations:
            sys.stderr.write(f"  - {violation}\n")
        sys.stderr.write("<system-reminder>→ Next: fail closed or route through the typed boundary; avoid silent fallback</system-reminder>\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
