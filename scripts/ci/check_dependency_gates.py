#!/usr/bin/env python3
"""Refuse a verification gate that passes because it did not run.

A skipped check and a passing check share an exit code, so a gate whose
dependency is absent reports success. That is how `check_public_api.sh` once
invented a baseline when none existed, and how a Postgres conformance test can
sit in the tree for weeks without ever reaching a database.

Two rules, both whole-repo scans with an exemption registry rather than a file
allow-list: a list freezes today's debt, a scan also refuses tomorrow's.

  R1  every crate whose tests short-circuit on an absent Postgres must appear
      in scripts/ci/pg_tests.sh, which is what sets AWAKEN_TEST_DATABASE_URL.
      A crate missing from it has tests that silently never run.
  R2  every short-circuit on an absent dependency must be registered, so the
      set of things CI is not covering is written down instead of implied.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PG_GATE = "scripts/ci/pg_tests.sh"

# Crates that reference a Postgres URL but legitimately need no gate entry.
# Key: crate directory name. Value: why its tests do not require a database.
PG_GATE_EXEMPTIONS: dict[str, str] = {}

# A skip whose dependency the Postgres gate supplies needs no entry here: R1
# is what makes "CI sets the variable" true, so R2 may rely on it. This registry
# is for the rest — a short-circuit on some other absent dependency. Keyed on
# the file, because a line number rots; the value says why green is sound.
SILENT_SKIP_EXEMPTIONS: dict[str, str] = {
    "crates/server/awaken-dispatch/tests/nats.rs": (
        "No CI gate starts a broker for a cargo test, so this one cannot run "
        "here. Real NATS wake is covered instead by the k3d group: "
        "check-all.sh:126 runs e2e/k3d/nats_wake_e2e.sh with "
        "AWAKEN_K3D_REQUIRED=1 against a live broker and a live Postgres "
        "dispatch owner. Registered rather than deleted so the unit-level gap "
        "is written down: if the k3d group is ever dropped, NATS wake loses "
        "its only real coverage and nothing else will say so."
    ),
    "crates/bin/awaken-sandbox/src/hand.rs": (
        "Same broker, same reason, same k3d coverage. The Hand's NATS path is "
        "exercised end to end there; this in-crate probe is a local-developer "
        "affordance."
    ),
}

SKIP = re.compile(r"\[skip\]|; skipping")
EXIT = re.compile(r"\breturn\b|\bNone\s*;|\bOk\(\(\)\)\s*;")
PG_MARK = re.compile(r"AWAKEN_TEST_DATABASE_URL|no Postgres|PgPool::connect")
GATE_ENTRY = re.compile(r"cargo test -p ([a-z0-9-]+)")
# The gate does not only supply Postgres. Whatever it exports, it supplies.
GATE_EXPORT = re.compile(r"^\s*export\s+(AWAKEN_TEST_[A-Z0-9_]+)=", re.M)


def _crate_of(relpath: str) -> str | None:
    parts = relpath.split("/")
    return parts[2] if len(parts) > 2 and parts[0] == "crates" else None


def _rust_sources(root: Path) -> dict[str, str]:
    return {
        path.relative_to(root).as_posix(): path.read_text(encoding="utf-8", errors="ignore")
        for path in sorted((root / "crates").rglob("*.rs"))
    }


def pg_gate_violations(sources: dict[str, str], gate: str) -> list[str]:
    covered = set(GATE_ENTRY.findall(gate))
    needing: dict[str, str] = {}
    for relpath, text in sources.items():
        crate = _crate_of(relpath)
        if crate and PG_MARK.search(text):
            needing.setdefault(crate, relpath)
    errors = []
    for crate, witness in sorted(needing.items()):
        if crate in covered or crate in PG_GATE_EXEMPTIONS:
            continue
        errors.append(
            f"{witness}: {crate} short-circuits without a database but is absent from "
            f"{PG_GATE}, so AWAKEN_TEST_DATABASE_URL is never set for it and its "
            "Postgres coverage silently never runs; add a pg_test line or record it "
            "in PG_GATE_EXEMPTIONS"
        )
    for crate in sorted(PG_GATE_EXEMPTIONS):
        if crate in needing or crate in covered:
            continue
        errors.append(
            f"{PG_GATE}: PG_GATE_EXEMPTIONS lists {crate}, which no longer references "
            "a database; drop the stale exemption"
        )
    return errors


def silent_skip_violations(sources: dict[str, str], gate: str) -> list[str]:
    covered = set(GATE_ENTRY.findall(gate))
    supplied_vars = set(GATE_EXPORT.findall(gate))
    errors = []
    seen: set[str] = set()
    for relpath, text in sources.items():
        lines = text.splitlines()
        for index, line in enumerate(lines):
            if not SKIP.search(line):
                continue
            window = "\n".join(lines[index : index + 4])
            if not EXIT.search(window):
                continue
            # R1 guarantees a gated crate actually receives the dependencies
            # the gate exports, so a short-circuit on one of those is a local
            # developer affordance, not a hole. A short-circuit on anything the
            # gate does not supply — a broker, a companion binary — still has
            # to be declared, because nothing makes it run.
            crate = _crate_of(relpath)
            supplied = crate is not None and crate in covered and any(
                var in text for var in supplied_vars
            )
            if supplied:
                continue
            seen.add(relpath)
            if relpath in SILENT_SKIP_EXEMPTIONS:
                continue
            errors.append(
                f"{relpath}:{index + 1}: a skipped dependency leaves this check green; "
                "either fail when its REQUIRE_* env is set, or record in "
                "SILENT_SKIP_EXEMPTIONS why passing without it is sound"
            )
    for relpath in sorted(SILENT_SKIP_EXEMPTIONS):
        if relpath not in seen:
            errors.append(
                f"{relpath}: SILENT_SKIP_EXEMPTIONS names a file that no longer "
                "short-circuits; drop the stale exemption"
            )
    return errors


def check_all(root: Path) -> list[str]:
    sources = _rust_sources(root)
    gate = (root / PG_GATE).read_text(encoding="utf-8")
    return pg_gate_violations(sources, gate) + silent_skip_violations(sources, gate)


def selftest() -> None:
    # Causes: C1 a crate short-circuits and is gated; C2 it short-circuits and is
    # absent; C3 it is absent but exempt; C4 an exemption names a crate that no
    # longer qualifies; C5 a skip is registered; C6 a skip is not.
    # Effects: E1 admitted, E2 rejected.
    # R1 = C1|C3|C5 => E1.  R2 = C2|C4|C6 => E2.
    global PG_GATE_EXEMPTIONS, SILENT_SKIP_EXEMPTIONS
    original = PG_GATE_EXEMPTIONS
    # The synthetic table must not inherit the real registry, or a live
    # exemption naming a crate absent from the fixture reads as a stale entry.
    PG_GATE_EXEMPTIONS = {}
    gated = "pg_test cargo test -p awaken-thing --lib\n"
    src = {"crates/stores/awaken-thing/src/lib.rs": 'let url = std::env::var("AWAKEN_TEST_DATABASE_URL");'}
    assert not pg_gate_violations(src, gated), "C1/E1"
    assert pg_gate_violations(src, "pg_test cargo test -p other\n"), "C2/E2"

    PG_GATE_EXEMPTIONS = {"awaken-thing": "test"}
    assert not pg_gate_violations(src, "pg_test cargo test -p other\n"), "C3/E1"
    PG_GATE_EXEMPTIONS = {"awaken-ghost": "test"}
    assert pg_gate_violations(src, "pg_test cargo test -p other\n"), "C4/E2"
    PG_GATE_EXEMPTIONS = original

    skipping = {"crates/a/b/src/x.rs": 'println!("[skip] no Postgres reachable");\n    return;'}
    assert silent_skip_violations(skipping, ""), "C6/E2"
    SILENT_SKIP_EXEMPTIONS = {"crates/a/b/src/x.rs": "test"}
    assert not silent_skip_violations(skipping, ""), "C5/E1"
    SILENT_SKIP_EXEMPTIONS = {}
    assert not silent_skip_violations(
        {"crates/a/awaken-thing/src/x.rs": 'AWAKEN_TEST_DATABASE_URL\nprintln!("[skip] no Postgres reachable");\n    return;'},
        "export AWAKEN_TEST_DATABASE_URL=x\npg_test cargo test -p awaken-thing",
    ), "C7/E1 a gated crate needs no entry for a dependency the gate supplies"
    assert silent_skip_violations(
        {"crates/a/awaken-thing/src/x.rs": 'no NATS reachable; skipping\n    return;'},
        "export AWAKEN_TEST_DATABASE_URL=x\npg_test cargo test -p awaken-thing",
    ), "C8/E2 a gated crate still owes an entry for a dependency it does not supply"
    SILENT_SKIP_EXEMPTIONS = {}
    print("check_dependency_gates selftest: the decision table holds")


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        selftest()
        raise SystemExit(0)
    problems = check_all(ROOT)
    for problem in problems:
        print(f"ERROR: {problem}", file=sys.stderr)
    if problems:
        raise SystemExit(1)
    print("OK - every dependency gate fails rather than skips.")
