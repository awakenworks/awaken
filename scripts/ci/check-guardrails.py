#!/usr/bin/env python3
"""Validate Awaken architecture guardrail entries.

The check keeps AGENTS.md as a short guardrail index: each hard rule must name
its ADR source, its mechanical enforcer, and its validation target.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path


GUARDRAIL_RE = re.compile(r"^- \*\*A-G(?P<num>\d+): (?P<title>[^*]+)\*\*", re.MULTILINE)
ADR_LINK_RE = re.compile(r"\[ADR-(?P<num>\d{4})\]\((?P<path>docs/adr/[^)]+\.md)\)")
PATH_RE = re.compile(r"`(?P<path>(?:crates|scripts|docs|apps)/[^`]+)`")
DOC_SECTION_RE = re.compile(
    r"^## Documentation Truth Sources\n(?P<body>.*?)(?=^## |\Z)",
    re.MULTILINE | re.DOTALL,
)
GUARDRAIL_SECTION_RE = re.compile(
    r"^## Architecture Guardrails\n(?P<body>.*?)(?=^## |\Z)",
    re.MULTILINE | re.DOTALL,
)

REQUIRED_TRUTH_PATHS = (
    "docs/adr/0035-published-versioned-registry-and-runtime-pinning.md",
    "docs/adr/0036-runtime-commit-atomicity-and-event-buffer.md",
    "docs/adr/0038-runtime-commit-boundary.md",
    "docs/adr/0039-run-activation-layering.md",
    "docs/adr/0040-resolver-resolved-run.md",
    "docs/adr/0019-mailbox-architecture.md",
    "docs/adr/0022-run-dispatch-data-model.md",
    "docs/adr/protocol-object-model-mapping.md",
)


@dataclass(frozen=True)
class Guardrail:
    num: int
    text: str


def _section(pattern: re.Pattern[str], text: str, name: str, errors: list[str]) -> str:
    match = pattern.search(text)
    if not match:
        errors.append(f"AGENTS.md missing required section: {name}")
        return ""
    return match.group("body")


def _guardrail_blocks(section: str) -> list[Guardrail]:
    matches = list(GUARDRAIL_RE.finditer(section))
    blocks: list[Guardrail] = []
    for index, match in enumerate(matches):
        start = match.start()
        end = matches[index + 1].start() if index + 1 < len(matches) else len(section)
        blocks.append(Guardrail(num=int(match.group("num")), text=section[start:end].strip()))
    return blocks


def _validate_existing_path(repo: Path, rel_path: str, errors: list[str]) -> None:
    if not (repo / rel_path).exists():
        errors.append(f"referenced path does not exist: {rel_path}")


def validate_text(repo: Path, text: str) -> list[str]:
    errors: list[str] = []

    doc_body = _section(DOC_SECTION_RE, text, "Documentation Truth Sources", errors)
    for rel_path in REQUIRED_TRUTH_PATHS:
        if rel_path not in doc_body:
            errors.append(f"Documentation Truth Sources missing {rel_path}")
        _validate_existing_path(repo, rel_path, errors)

    guardrail_body = _section(GUARDRAIL_SECTION_RE, text, "Architecture Guardrails", errors)
    guardrails = _guardrail_blocks(guardrail_body)
    if len(guardrails) < 8:
        errors.append("Architecture Guardrails must list at least 8 hard rules")
        return errors

    seen: set[int] = set()
    for guardrail in guardrails:
        if guardrail.num in seen:
            errors.append(f"duplicate guardrail id A-G{guardrail.num}")
        seen.add(guardrail.num)

        for label in ("Source:", "Enforcer:", "Validation:"):
            if label not in guardrail.text:
                errors.append(f"A-G{guardrail.num} missing {label}")

        adr_links = list(ADR_LINK_RE.finditer(guardrail.text))
        if not adr_links:
            errors.append(f"A-G{guardrail.num} must link at least one ADR source")
        for link in adr_links:
            _validate_existing_path(repo, link.group("path"), errors)

        if "Enforcer:" in guardrail.text:
            enforcer_text = guardrail.text.split("Enforcer:", 1)[1].split("Validation:", 1)[0].strip()
            if len(enforcer_text) < 12:
                errors.append(f"A-G{guardrail.num} has an empty enforcer")

        if "Validation:" in guardrail.text:
            validation_text = guardrail.text.split("Validation:", 1)[1].strip()
            if len(validation_text) < 8:
                errors.append(f"A-G{guardrail.num} has an empty validation target")
            for path_match in PATH_RE.finditer(validation_text):
                rel_path = path_match.group("path")
                if rel_path.endswith(".rs") or rel_path.endswith(".py") or rel_path.endswith(".md"):
                    _validate_existing_path(repo, rel_path, errors)

    expected = list(range(1, max(seen) + 1))
    actual = sorted(seen)
    if actual != expected:
        errors.append(f"guardrail ids must be contiguous from A-G1: got {actual}")

    return errors


def self_test() -> int:
    repo = Path.cwd()
    sample = """## Documentation Truth Sources
- Registry pinning: docs/adr/0035-published-versioned-registry-and-runtime-pinning.md
- Runtime events: docs/adr/0036-runtime-commit-atomicity-and-event-buffer.md
- Commit boundary: docs/adr/0038-runtime-commit-boundary.md
- Activation: docs/adr/0039-run-activation-layering.md
- Resolution: docs/adr/0040-resolver-resolved-run.md
- Mailbox: docs/adr/0019-mailbox-architecture.md
- Dispatch: docs/adr/0022-run-dispatch-data-model.md
- Protocols: docs/adr/protocol-object-model-mapping.md

## Architecture Guardrails
- **A-G1: One.** Source: [ADR-0038](docs/adr/0038-runtime-commit-boundary.md). Enforcer: validated entry point. Validation: existing checks.
- **A-G2: Two.** Source: [ADR-0035](docs/adr/0035-published-versioned-registry-and-runtime-pinning.md). Enforcer: fail-closed materialization. Validation: existing checks.
- **A-G3: Three.** Source: [ADR-0040](docs/adr/0040-resolver-resolved-run.md). Enforcer: typed run plan. Validation: existing checks.
- **A-G4: Four.** Source: [ADR-0036](docs/adr/0036-runtime-commit-atomicity-and-event-buffer.md). Enforcer: staged capture. Validation: existing checks.
- **A-G5: Five.** Source: [ADR-0019](docs/adr/0019-mailbox-architecture.md). Enforcer: state machine. Validation: existing checks.
- **A-G6: Six.** Source: [ADR-0022](docs/adr/0022-run-dispatch-data-model.md). Enforcer: dispatch validator. Validation: existing checks.
- **A-G7: Seven.** Source: [ADR-0039](docs/adr/0039-run-activation-layering.md). Enforcer: activation validator. Validation: existing checks.
- **A-G8: Eight.** Source: [ADR-0038](docs/adr/0038-runtime-commit-boundary.md). Enforcer: dependency checks. Validation: existing checks.
"""
    errors = validate_text(repo, sample)
    if errors:
        sys.stderr.write("self-test unexpectedly failed:\n" + "\n".join(errors) + "\n")
        return 1

    broken = sample.replace("Enforcer: validated entry point. ", "")
    broken_errors = validate_text(repo, broken)
    if not any("A-G1 missing Enforcer" in error for error in broken_errors):
        sys.stderr.write("self-test did not catch a missing enforcer\n")
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--repo", default=".")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    repo = Path(args.repo).resolve()
    agents = repo / "AGENTS.md"
    if not agents.exists():
        sys.stderr.write("AGENTS.md not found\n")
        return 1

    errors = validate_text(repo, agents.read_text(encoding="utf-8"))
    if errors:
        sys.stderr.write("ERROR: architecture guardrail contract failed:\n")
        for error in errors:
            sys.stderr.write(f"  - {error}\n")
        sys.stderr.write("<system-reminder>→ Next: add ADR source, enforcer, and validation for every guardrail</system-reminder>\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
