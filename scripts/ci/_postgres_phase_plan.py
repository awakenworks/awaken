"""Derive native PostgreSQL qualification phases from the authored selectors.

Only literal required-test calls are admitted. Unknown shell syntax refuses
planning instead of producing a partial list that could qualify an omitted
phase. This module never executes a selector, Cargo or a service command.
"""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import shlex
import sys

INPUT_GLOBS = ("scripts/ci/pg_tests.sh",)
ROOT = Path(__file__).resolve().parents[2]
SELECTOR = "scripts/ci/pg_tests.sh"


@dataclass(frozen=True)
class PostgresPhase:
    selector: str
    ordinal: int
    arguments: tuple[str, ...]
    environment: tuple[tuple[str, str], ...]

    @property
    def package(self) -> str:
        return self.arguments[self.arguments.index("-p") + 1]

    @property
    def identity(self) -> str:
        return f"{self.selector}:{self.ordinal}:{self.package}"


def selector_phases(source: str, selector: str) -> tuple[PostgresPhase, ...]:
    if not re.fullmatch(r"[a-z_]+", selector):
        raise ValueError("invalid PostgreSQL selector name")
    matches = list(re.finditer(r"(?ms)^" + selector + r"\(\) \{\n(.*?)^\}", source))
    if len(matches) != 1:
        raise ValueError(f"{SELECTOR}: expected exactly one {selector} definition")
    body = re.sub(r"\\\n[ \t]*", " ", matches[0].group(1))
    phases, composed = [], []
    for line_number, line in enumerate(body.splitlines(), 1):
        location = f"{SELECTOR}:{selector}:statement {line_number}"
        try:
            words = shlex.split(line, comments=True)
        except ValueError as error:
            raise ValueError(location + ": unsupported literal command shape") from error
        if not words:
            continue
        environment = ()
        if words[0] == "AWAKEN_CRASH_BACKEND=postgres":
            environment = (("AWAKEN_CRASH_BACKEND", "postgres"),)
            words = words[1:]
        if not words or words[0] != "pg_required_test":
            raise ValueError(location + ": only literal pg_required_test calls are admitted")
        guarded = words[-3:] == ["||", "record_failure", "$?"]
        arguments = words[1:-3] if guarded else words[1:]
        # No expansion, pipelines, substitution, globbing or shell statements.
        # Diagnostic text deliberately omits argument values and credentials.
        if any(any(character in word for character in "$`;|&<>*?[]~\n") for word in arguments):
            raise ValueError(location + ": dynamic or compound shell argument is not admitted")
        frontend = arguments[:arguments.index("--")] if "--" in arguments else arguments
        if frontend.count("-p") != 1:
            raise ValueError(location + ": exactly one literal package is required")
        package_index = frontend.index("-p") + 1
        if package_index >= len(frontend) or not re.fullmatch(r"[a-z][a-z0-9-]*", frontend[package_index]):
            raise ValueError(location + ": invalid package declaration")
        floors = [word.removeprefix("--require-case=") for word in frontend if word.startswith("--require-case=")]
        if not floors or any(not word for word in floors):
            raise ValueError(location + ": nonempty required case floors are mandatory")
        phases.append(PostgresPhase(selector, len(phases) + 1, tuple(arguments), environment))
        composed.append(guarded)
    if not phases:
        raise ValueError(f"{SELECTOR}:{selector}: required phase selection is empty")
    if len(phases) > 1 and not all(composed):
        raise ValueError(f"{SELECTOR}:{selector}: every multi-phase failure must be composed")
    return tuple(phases)


def read_phases(root: Path, selectors: tuple[str, ...]) -> tuple[PostgresPhase, ...]:
    # Read the subject afresh; filesystem path/mtime is never a cache key.
    source = (root / SELECTOR).read_text(encoding="utf-8")
    return tuple(phase for selector in selectors for phase in selector_phases(source, selector))


def self_test(root: Path) -> None:
    call = "pg_required_test -p package --lib --require-case=case -- --test-threads=1"
    source = "selected() {\n  " + call + "\n}\n"
    one = selector_phases(source, "selected")
    assert len(one) == 1 and one[0].package == "package", "P1 literal terminal selector"
    assert not one[0].environment
    guarded = call + ' || record_failure "$?"'
    two = selector_phases("selected() {\n  " + guarded + "\n  " + guarded + "\n}\n", "selected")
    assert len(two) == 2 and two[0].identity != two[1].identity, "P2 preserve phase multiplicity"
    pg = selector_phases(source.replace("  pg_required_test", "  AWAKEN_CRASH_BACKEND=postgres pg_required_test"), "selected")
    assert pg[0].environment == (("AWAKEN_CRASH_BACKEND", "postgres"),), "P3 backend is execution input"
    for mutant in (
        source + source, source.replace(call, ""), source.replace("-p package", "-p $PACKAGE"),
        source.replace(call, call + "\n  " + call), source.replace("--require-case=case", ""),
        source.replace("pg_required_test", "ordinary_test"), source.replace("-p package", "-p package; touch ignored"),
        source.replace(call, "if true; then " + call + "; fi"),
        source.replace("  pg_required_test", "  AWAKEN_CRASH_BACKEND=sqlite pg_required_test"),
    ):
        try:
            selector_phases(mutant, "selected")
        except ValueError:
            pass
        else:
            raise AssertionError("P4 unsupported or incomplete phase must refuse")
    sentinel = "SENTINEL-not-for-diagnostics"
    try:
        selector_phases(source.replace(call, "UNSUPPORTED_ASSIGNMENT=" + sentinel + " " + call), "selected")
    except ValueError as error:
        assert sentinel not in str(error), "P5 no argument material in diagnostics"
    else:
        raise AssertionError("P5 unsupported assignment must refuse")
    selectors = ("backend_parity", "work_store_evidence", "http_transport_evidence",
                 "critical_durability_evidence", "storage_observation_evidence", "resources_evidence", "storage_evidence")
    phases = read_phases(root, selectors)
    assert len(phases) == 38, "P6 current required selector inventory"
    startup = [phase for phase in phases if phase.package == "awaken-process-startup"]
    assert len(startup) == 2 and startup[0].arguments != startup[1].arguments, "P7 distinct process bootstrap phases"
    assert all(phase.environment == (("AWAKEN_CRASH_BACKEND", "postgres"),)
               for phase in phases if phase.package in ("awaken-config-store", "awaken-session-store", "awaken-credential-store")
               and phase.selector == "critical_durability_evidence"), "P8 crash backend overrides"
    print("PostgreSQL phase plan: literal calls, refusals and current selectors passed")


def main(arguments: list[str]) -> int:
    if arguments == ["--self-test"]:
        self_test(ROOT)
        return 0
    print("usage: _postgres_phase_plan.py --self-test", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
