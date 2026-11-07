"""Native PostgreSQL qualification phases, read from the suite table.

A phase is one pg_required_test row of scripts/ci/service_suites.toml whose
`phase` is a named selector (backend_parity, storage_evidence, ...). The table's
schema (service_test_plan.py) already admits only literal words; a native phase
also needs exactly one -p, at least one --require-case and only the fixed
environment the native runner reproduces. Anything else refuses planning instead
of producing a partial list. This module never executes a selector, Cargo or a
service command.
"""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import sys

from service_test_plan import load

INPUT_GLOBS = ("scripts/ci/service_suites.toml",)
ROOT = Path(__file__).resolve().parents[2]
SELECTOR = "scripts/ci/service_suites.toml"
ENVIRONMENT = (("AWAKEN_CRASH_BACKEND", "postgres"), ("AWAKEN_SESSION_PERF_ROOTS", "100000"))


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


def selector_phases(table: str, selector: str) -> tuple[PostgresPhase, ...]:
    if not re.fullmatch(r"[a-z_]+", selector):
        raise ValueError("invalid PostgreSQL selector name")
    phases = []
    for row in load(table):
        if row.group != "postgres" or row.phase != selector:
            continue
        location = f"{SELECTOR}:{row.id}"
        # Diagnostics name the row, never argument values: a value may hold a credential.
        if row.runner != "pg_required_test":
            raise ValueError(location + ": only pg_required_test rows are admitted")
        if len(row.env) > 1 or any(pair not in ENVIRONMENT for pair in row.env):
            raise ValueError(location + ": only the fixed native environment is admitted")
        frontend = row.args[:row.args.index("--")] if "--" in row.args else row.args
        if frontend.count("-p") != 1:
            raise ValueError(location + ": exactly one literal package is required")
        package_index = frontend.index("-p") + 1
        if package_index >= len(frontend) or not re.fullmatch(r"[a-z][a-z0-9-]*", frontend[package_index]):
            raise ValueError(location + ": invalid package declaration")
        floors = [word.removeprefix("--require-case=") for word in frontend if word.startswith("--require-case=")]
        if not floors or any(not word for word in floors):
            raise ValueError(location + ": nonempty required case floors are mandatory")
        phases.append(PostgresPhase(selector, len(phases) + 1, row.args, row.env))
    if not phases:
        raise ValueError(f"{SELECTOR}:{selector}: required phase selection is empty")
    return tuple(phases)


def edit_row(table: str, ident: str, transform) -> str:
    """Test support: ``table`` with row ``ident``'s args rewritten, as one line of words, by ``transform``."""
    found = re.search(r'(?ms)^id = "' + re.escape(ident) + r'"\n(?:(?!^\[\[).)*?^args = \'\'\'\n(.*?)\'\'\'', table)
    if found is None:
        raise ValueError(f"{SELECTOR}: no row {ident} with multi-line args")
    words = " ".join(found.group(1).split()) + " "
    changed = transform(words)
    if changed == words:
        raise ValueError(f"{SELECTOR}: the edit left row {ident} unchanged")
    return table[:found.start(1)] + changed.strip() + "\n" + table[found.end(1):]


def row_block(table: str, ident: str) -> str:
    """Test support: the whole ``[[group]]`` block of row ``ident``, up to the next row or comment."""
    found = re.search(r'(?ms)^\[\[\w+\]\]\nid = "' + re.escape(ident) + r'"\n.*?(?=^\[\[|^#|\Z)', table)
    if found is None:
        raise ValueError(f"{SELECTOR}: no row {ident}")
    return found.group(0)


def read_phases(root: Path, selectors: tuple[str, ...]) -> tuple[PostgresPhase, ...]:
    # Read the subject afresh; filesystem path/mtime is never a cache key.
    table = (root / SELECTOR).read_text(encoding="utf-8")
    return tuple(phase for selector in selectors for phase in selector_phases(table, selector))


def self_test(root: Path) -> None:
    row = "[[postgres]]\nid = \"postgres.{name}\"\nphase = \"selected\"\n{env}args = '{args}'\n"
    args = "-p package --lib --require-case=case -- --test-threads=1"

    def table(*rows: tuple[str, str, str]) -> str:
        return "\n".join(row.format(name=name, env=env, args=words) for name, env, words in rows)

    one = selector_phases(table(("a", "", args)), "selected")
    assert len(one) == 1 and one[0].package == "package" and not one[0].environment, "P1 literal terminal selector"
    two = selector_phases(table(("a", "", args), ("b", "", args)), "selected")
    assert len(two) == 2 and two[0].identity != two[1].identity, "P2 preserve phase multiplicity"
    crash = 'env = { AWAKEN_CRASH_BACKEND = "postgres" }\n'
    assert selector_phases(table(("a", crash, args)), "selected")[0].environment == (ENVIRONMENT[0],), "P3 backend"
    capacity = 'env = { AWAKEN_SESSION_PERF_ROOTS = "100000" }\n'
    assert selector_phases(table(("a", capacity, args)), "selected")[0].environment == (ENVIRONMENT[1],), "P9 capacity"
    for mutant in (
        table(("a", capacity.replace("100000", "10000"), args)), table(("a", crash.replace("postgres", "sqlite"), args)),
        table(("a", "", args), ("a", "", args)), table(("a", "", args.replace("selected", "other"))).replace("selected", "other"),
        table(("a", "", args.replace("-p package", "-p $PACKAGE"))), table(("a", "", args.replace("--require-case=case", ""))),
        table(("a", "runner = \"pg_test\"\n", "cargo test -p package --require-case=case")),
        table(("a", "", args.replace("-p package", "-p package; touch ignored"))),
        table(("a", "", args.replace("-p package", "-p package -p other"))),
    ):
        try:
            selector_phases(mutant, "selected")
        except ValueError:
            pass
        else:
            raise AssertionError("P4 unsupported or incomplete phase must refuse")
    sentinel = "SENTINEL-not-for-diagnostics"
    try:
        selector_phases(table(("a", f'env = {{ UNSUPPORTED = "{sentinel}" }}\n', args)), "selected")
    except ValueError as error:
        assert sentinel not in str(error), "P5 no argument material in diagnostics"
    else:
        raise AssertionError("P5 unsupported environment must refuse")
    selectors = ("backend_parity", "work_store_evidence", "http_transport_evidence",
                 "critical_durability_evidence", "storage_observation_evidence", "resources_evidence", "storage_evidence")
    phases = read_phases(root, selectors)
    assert len(phases) == 39, "P6 current required selector inventory"
    startup = [phase for phase in phases if phase.package == "awaken-process-startup"]
    assert len(startup) == 2 and startup[0].arguments != startup[1].arguments, "P7 distinct process bootstrap phases"
    assert all(phase.environment == (ENVIRONMENT[0],)
               for phase in phases if phase.package in ("awaken-config-store", "awaken-session-store", "awaken-credential-store")
               and phase.selector == "critical_durability_evidence"), "P8 crash backend overrides"
    print("PostgreSQL phase plan: table rows, refusals and current selectors passed")


def main(arguments: list[str]) -> int:
    if arguments == ["--self-test"]:
        self_test(ROOT)
        return 0
    print("usage: _postgres_phase_plan.py --self-test", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
