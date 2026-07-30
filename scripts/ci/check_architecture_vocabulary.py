#!/usr/bin/env python3
"""Keep active architecture documents on one service-boundary vocabulary.

Historical ADR text is append-only and intentionally excluded. ADR-0071 and the
current design documents own the replacement vocabulary; navigation and wiki
documents may link to those owners but may not revive a retired implementation
path.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
DOCS = REPO_ROOT / "docs"
CURRENT_ADR = DOCS / "adr" / (
    "0071-distributed-service-boundaries-and-executable-agent-registration.md"
)

FORBIDDEN = {
    "retired bundled run input": re.compile(r"\bRunnableConfig\b"),
    "retired publication value": re.compile(r"\bRegistryPublication\b"),
    "nonexistent registry compiler role": re.compile(r"\bRegistryCompiler\b"),
    "nonexistent publication coordinator role": re.compile(
        r"\bConfigPublicationCoordinator\b"
    ),
    "retired whole-catalog command": re.compile(r"\bRuntimeCatalogInstall\b"),
    "retired whole-catalog port": re.compile(r"\bRuntimeCatalogInstaller\b"),
    "ambiguous publication installer": re.compile(r"\bAgentPublicationInstaller\b"),
    "implementation-shaped publication projector": re.compile(
        r"\bAgentPublicationProjector(?:\.project)?\b"
    ),
    "retired process-local boundary verb": re.compile(
        r"\bInstalledAgentCatalog\.install\b"
    ),
}

CANONICAL_REQUIREMENTS = {
    Path("docs/design/config-publication-lifecycle.md"): (
        "ExecutableAgentRegistrar",
        "register",
        "StoredPublication",
        "ExecutableAgentCatalogRepository",
    ),
    Path("docs/design/config-to-run-execution-flow.md"): (
        "## Flow One: Configuration To Application",
        "## Flow Two: Request To Complete Response",
        "### Reused unchanged",
        "### Modified",
        "### New boundary code",
    ),
    Path(
        "docs/adr/0071-distributed-service-boundaries-and-executable-agent-registration.md"
    ): (
        "ExecutableAgentRegistrar::register",
        "deployment_run_id",
        "No whole-catalog installation track",
    ),
}

ENGLISH_ONLY = set(CANONICAL_REQUIREMENTS) | {
    Path("docs/design/architecture-overview.md"),
    Path("docs/design/managed-deployments.md"),
    Path("docs/design/resources-memory-files-skills.md"),
    Path("docs/design/credentials-and-vaults.md"),
    Path("docs/wiki/config-to-run-execution-flow-facts.md"),
}

CJK = re.compile(r"[\u3400-\u4dbf\u4e00-\u9fff]")


def active_markdown() -> list[Path]:
    return [
        path
        for path in sorted(DOCS.rglob("*.md"))
        if ("adr" not in path.relative_to(DOCS).parts or path == CURRENT_ADR)
        and path != DOCS / "wiki" / "log.md"
    ]


def vocabulary_violations(path: Path, text: str) -> list[str]:
    violations: list[str] = []
    for label, pattern in FORBIDDEN.items():
        for match in pattern.finditer(text):
            line = text.count("\n", 0, match.start()) + 1
            violations.append(f"{path}:{line}: {label}: `{match.group(0)}`")
    return violations


def repository_violations() -> list[str]:
    violations: list[str] = []
    for path in active_markdown():
        violations.extend(
            vocabulary_violations(
                path.relative_to(REPO_ROOT), path.read_text(encoding="utf-8")
            )
        )

    for relative, required in CANONICAL_REQUIREMENTS.items():
        path = REPO_ROOT / relative
        if not path.is_file():
            violations.append(f"{relative}: missing canonical architecture owner")
            continue
        text = path.read_text(encoding="utf-8")
        for term in required:
            if term not in text:
                violations.append(f"{relative}: missing canonical term `{term}`")

    for relative in ENGLISH_ONLY:
        path = REPO_ROOT / relative
        if not path.is_file():
            continue
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if CJK.search(line):
                violations.append(f"{relative}:{number}: canonical documentation must be English")
    return violations


def self_test() -> int:
    # Cause/effect graph and decision table:
    # R1 active text + retired type(s) -> one violation per retired type;
    # R2 active text + canonical register vocabulary -> no violation;
    # R3 historical handling is implemented by active_markdown exclusion, so
    #    this unit tests the scanner only and the repository check proves the
    #    retained ADR text does not enter the active set.
    cases = (
        ("RunnableConfig bundles an install.", 1, "R1"),
        ("ConfigPublicationCoordinator invokes RegistryCompiler.", 2, "R1"),
        ("RuntimeCatalogInstaller accepts a command.", 1, "R1"),
        ("RuntimeCatalogInstall is durable.", 1, "R1"),
        ("AgentPublicationProjector.project(snapshot)", 1, "R1"),
        ("ExecutableAgentRegistrar.register(snapshot)", 0, "R2"),
    )
    failures: list[str] = []
    for text, expected, rule in cases:
        actual = len(vocabulary_violations(Path("synthetic.md"), text))
        if actual != expected:
            failures.append(f"{rule}: expected {expected} violation(s), got {actual}: {text}")
    if failures:
        print("check-architecture-vocabulary self-test:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    return 0


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        return self_test()
    if argv:
        print("usage: check_architecture_vocabulary.py [--self-test]", file=sys.stderr)
        return 2
    violations = repository_violations()
    if violations:
        print("check-architecture-vocabulary:", file=sys.stderr)
        for violation in violations:
            print(f"  {violation}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
