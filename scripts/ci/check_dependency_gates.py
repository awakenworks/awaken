#!/usr/bin/env python3
"""Refuse a verification gate that passes because it did not run.

A skipped check and a passing check share an exit code, so a gate whose
dependency is absent reports success. That is how `check_public_api.sh` once
invented a baseline when none existed, and how a Postgres conformance test can
sit in the tree for weeks without ever reaching a database.

Three rules, all whole-repo scans with an exemption registry rather than a file
allow-list: a list freezes today's debt, a scan also refuses tomorrow's.

  R1  every crate whose tests short-circuit on an absent Postgres must appear
      in scripts/ci/pg_tests.sh, which is what sets AWAKEN_TEST_DATABASE_URL.
      A crate missing from it has tests that silently never run.
  R2  every short-circuit on an absent dependency must be covered by a gate or
      registered, so the set of things CI is not covering is written down
      instead of implied.
  R3  a test that decides whether to run by reading an environment variable may
      only read a variable something in this repository supplies. `pg_notify_wake.rs`
      read `AWAKEN_TEST_PG_URL` while the Postgres gate exported
      `AWAKEN_TEST_DATABASE_URL`, so it printed `skip:` on every run this
      repository has ever made, and nothing said so. One substrate, one spelling,
      and the supplier is the authority on what that spelling is.

The scan is not limited to `crates/**`, and the skip vocabulary is not limited to
one phrase: the live instance said `skip:` and lived under a regex that only knew
`[skip]` and `; skipping`. A rule that only recognises the spellings already
written down cannot refuse the next one.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PG_GATE = "scripts/ci/pg_tests.sh"

# The gates that stand a substrate up, and how each one covers the tests it runs.
#
#   "exports"       the gate supplies dependencies as environment variables, so a
#                   test it runs that short-circuits on one of those is a local
#                   developer affordance: in the gate, the variable is set.
#   "refuses-skip"  the gate has a mode that fails on any skip at all, so a test
#                   it runs cannot silently not-run there. That claim is checked
#                   structurally below, because a declaration nobody verifies is
#                   exactly the thing this file exists to refuse.
SUBSTRATE_GATES: tuple[tuple[str, str], ...] = (
    (PG_GATE, "exports"),
    ("scripts/e2e/k8s_container_e2e.sh", "exports"),
    ("scripts/e2e/sandbox_capability_suite.sh", "refuses-skip"),
)
# What "refuses-skip" has to look like in the gate's own text.
SKIP_REFUSAL = re.compile(r'if \[ "\$require_all" -eq 1 \] && \[ "\$SKIP" -ne 0 \]')

# Where a short-circuit can hide. `crates/**` was the whole corpus, so a runner
# under e2e/ or a gate script under scripts/ could skip in silence.
SOURCE_TREES: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("crates", ("*.rs",)),
    ("e2e", ("*.mjs", "*.ts", "*.sh")),
    ("scripts", ("*.sh", "*.py")),
)
EXCLUDED_PARTS = frozenset({"node_modules", "target", "__pycache__", ".git"})

# Crates that reference a Postgres URL but legitimately need no gate entry.
# Key: crate directory name. Value: why its tests do not require a database.
PG_GATE_EXEMPTIONS: dict[str, str] = {
    "awaken-reliability-testkit": (
        "Owns only the pure three-state AWAKEN_TEST_DATABASE_URL admission helper. "
        "Its five database-owning consumers remain individually listed in pg_tests.sh; "
        "the testkit has no schema, driver, connection, or Postgres test to run."
    ),
}

# A skip whose dependency the Postgres gate supplies needs no entry here: R1
# is what makes "CI sets the variable" true, so R2 may rely on it. This registry
# is for the rest — a short-circuit on some other absent dependency. Keyed on
# the file, because a line number rots; the value says why green is sound.
SILENT_SKIP_EXEMPTIONS: dict[str, str] = {
    # ---- substrates no lane stands up -----------------------------------
    "crates/server/awaken-dispatch/tests/nats.rs": (
        "No CI gate starts a broker for a cargo test, so this one cannot run "
        "here. Real NATS wake is covered instead by the k3d group: "
        "check-all.sh runs e2e/k3d/nats_wake_e2e.sh with AWAKEN_K3D_REQUIRED=1 "
        "against a live broker and a live Postgres dispatch owner. Registered "
        "rather than deleted so the unit-level gap is written down: if the k3d "
        "group is ever dropped, NATS wake loses its only real coverage and "
        "nothing else will say so."
    ),
    "crates/runtime/awaken-ext-mcp/tests/http_real_server.rs": (
        "Points at an external MCP server through MCP_TEST_URL/MCP_TEST_BEARER. "
        "Nothing in this repository stands one up and no lane supplies those "
        "credentials, so this never runs. The HTTP transport it exercises is "
        "covered hermetically by e2e/mcp_streamable_http_e2e.ts against the "
        "repository's own MCP server; what is uncovered is interoperability "
        "with a third-party implementation."
    ),
    "crates/worker/awaken-execution-composition/src/container.rs": (
        "L7 needs a rootless Podman that can start busybox, and no lane stands "
        "Podman up: the sandbox suite's container layer is Docker only. While "
        "this test lived in awaken-session-executor the check counted it covered "
        "because the suite runs other tests of that crate, yet no lane ever ran "
        "it with Podman; moving it here only made the gap visible. The L1-L7 "
        "lease decisions are covered hermetically by awaken-sandbox-container's "
        "pool/lease_tests.rs and warm hand-out against real Docker by "
        "tests/pool_docker.rs; what is uncovered is the production builder "
        "under real Podman."
    ),
    # ---- e2e runners whose substrate is absent by default ----------------
    "e2e/acp_projected_container_e2e.mjs": (
        "Requires a reachable Docker daemon. The sandbox group covers the "
        "container tier through scripts/e2e/sandbox_capability_suite.sh "
        "--require-substrates, which refuses to skip; this projected-container "
        "variant is not in that suite's list."
    ),
    "e2e/acp_real_mcp_kimi_e2e.mjs": (
        "Requires a live KIMI credential read from the developer's shell "
        "profile. No lane has one. The same ACP+MCP wiring is covered "
        "deterministically by e2e/acp_managed_mcp_e2e.mjs."
    ),
    "e2e/bwrap_capability.mjs": (
        "This file IS the capability probe: returning false is its answer, not "
        "a skipped assertion. Its callers decide what to do with the answer, "
        "and scripts/e2e/sandbox_capability_suite.sh --require-substrates turns "
        "a false answer into a failed gate."
    ),
    "e2e/conformance/when_online_smoke_e2e.mjs": (
        "Deliberately inert without real-API credentials and fixture ids "
        "(AWAKEN_WHEN_ONLINE). No lane supplies a live Anthropic account. The "
        "same wire contract is covered against the recorded oracle by the "
        "managed-sdk-oracle check in the contracts group."
    ),
    "e2e/durable_pg_commit_e2e.mjs": (
        "Reads SESSION_DEPLOYMENT_DATABASE_URL, which no lane sets: "
        "scripts/ci/pg_tests.sh exports AWAKEN_TEST_DATABASE_URL and runs Rust "
        "suites, not these Node runners. Durable commit against a real "
        "Postgres is covered by pg_tests.sh's awaken-dispatch and "
        "awaken-session-store suites; what is uncovered is the same path "
        "driven through the served HTTP process."
    ),
    "e2e/durable_pg_wake_e2e.mjs": (
        "Same unset variable, same gap, same Rust-level coverage: the wake path "
        "is exercised by pg_tests.sh's awaken-dispatch suite and by the k3d "
        "NATS wake runner."
    ),
    "e2e/postgres_claimed_commit_guard_e2e.ts": (
        "Same unset variable. The claim/commit guard itself is proved by "
        "pg_tests.sh's durable_postgres and active_active_postgres suites, "
        "which the Postgres gate names explicitly."
    ),
    "e2e/hand_brain_remote_worker_e2e.ts": (
        "Requires a reachable container runtime for the selected tier. The "
        "sandbox group covers the Hand/Brain split against real substrates "
        "through scripts/e2e/sandbox_capability_suite.sh --require-substrates."
    ),
    "e2e/managed_container_agent_e2e.mjs": (
        "Requires a reachable container engine; covered under the sandbox "
        "group, which refuses to skip when the substrate is demanded."
    ),
    "e2e/managed_package_registry_e2e.mjs": (
        "Requires Docker to stand up a package registry. No deterministic lane "
        "provides one; the registry client itself is covered by unit tests in "
        "awaken-environment-image-build."
    ),
    "e2e/trace_backends_e2e.mjs": (
        "Requires the Jaeger/Phoenix stack in e2e/phoenix/docker-compose.yml, "
        "which no lane brings up. OTLP export is covered hermetically by "
        "e2e/trace_capture_e2e.mjs against a collector fixture; what is "
        "uncovered is the real backends' ingestion."
    ),
    "e2e/k3d/harness.sh": (
        "The admission helper exits zero when k3d/kubectl/docker are absent, "
        "which is the developer affordance. Every lane invocation passes "
        "AWAKEN_K3D_REQUIRED=1, under which the same condition fails instead."
    ),
    # ---- gate scripts whose skip a lane already escalates ----------------
    "scripts/ci/pg_tests.sh": (
        "Docker absence is a skip only without --require-docker. check-all.sh "
        "passes --require-docker in the postgres group, so the release lane "
        "fails on the same condition."
    ),
    "scripts/ci/check-rust.sh": (
        "The 'no Cargo.toml' branch cannot fire in this repository, which has "
        "one; it exists so the script is honest when vendored into a checkout "
        "that does not. A missing toolchain, by contrast, fails."
    ),
    "scripts/ci/check-frontend.sh": (
        "The 'no package.json' branch cannot fire in this repository, for the "
        "same reason and with the same shape as check-rust.sh's."
    ),
    "scripts/ci/check_dependency_gates.py": (
        "This checker's own rule text names every skip spelling it refuses. "
        "Scanning itself would make stating the rule a violation of it."
    ),
}

# Every spelling of "I am not running" this repository has used, plus the shapes
# next to them. The live defect said `skip:`; the previous regex knew `[skip]`
# and `; skipping`, so it read the file and found nothing.
SKIP = re.compile(
    r"\[skip\]|\[SKIP\]|;\s*skipping\b|\bskip:|\bSKIP:|\bskipped:|\bskipping\b",
)
EXIT = re.compile(r"\breturn\b|\bNone\s*;|\bOk\(\(\)\)\s*;|\bprocess\.exit\(0\)|\bexit 0\b")
PG_MARK = re.compile(r"AWAKEN_TEST_DATABASE_URL|no Postgres|PgPool::connect")
# `-q`, `--features`, a package filter: the package is what matters, wherever it
# sits on the line.
GATE_ENTRY = re.compile(
    r"\b(?:cargo (?:test|nextest run)|pg_required_test|run_required_rust_tests)\b[^\n]*?\s-p\s+([a-z0-9-]+)"
)
# The gate does not only supply Postgres, and it does not only `export`: a
# `VAR=1 cargo test ...` prefix supplies the variable for exactly that command.
# Whatever a gate sets, it supplies.

# The variable a skip names as the thing it was waiting for.
SWITCH_NAME = re.compile(r"\b(?:AWAKEN_[A-Z0-9_]+|SESSION_[A-Z0-9_]+|MCP_TEST_[A-Z0-9_]+)\b")
# How something in this repository supplies one.
ENVIRONMENT_SUPPLY = re.compile(
    r"""(?:^|[\s,{(])(AWAKEN_[A-Z0-9_]+|SESSION_[A-Z0-9_]+|MCP_TEST_[A-Z0-9_]+)\s*[=:](?!=)""",
    re.M,
)


COMMENT = re.compile(r"^\s*(#|//|\*|/\*)")


def _executable_lines(text: str) -> list[str]:
    """The lines that run, with comments blanked out.

    A comment that says "skipping" is documentation. Counting it as a skip makes
    every checker that *describes* this rule violate it, and buries the real
    short-circuits under prose.
    """

    return ["" if COMMENT.match(line) else line for line in text.splitlines()]


CRATE_TREES = frozenset({"src", "tests", "benches", "examples"})


def _crate_of(relpath: str) -> str | None:
    """The crate a repository path belongs to, at whatever directory depth.

    On disk the nearest ancestor holding a `Cargo.toml` is the crate. A path
    that does not exist (the decision table) names its crate by the directory
    holding its source tree.
    """
    parts = relpath.split("/")
    if len(parts) < 3 or parts[0] != "crates":
        return None
    for end in range(len(parts) - 1, 1, -1):
        if (ROOT.joinpath(*parts[:end]) / "Cargo.toml").is_file():
            return parts[end - 1]
    for index in range(2, len(parts)):
        if parts[index] in CRATE_TREES:
            return parts[index - 1]
    return None


def _sources(root: Path) -> dict[str, str]:
    """Every file a short-circuit can hide in, not only the Rust crates."""

    sources: dict[str, str] = {}
    for tree, patterns in SOURCE_TREES:
        base = root / tree
        if not base.is_dir():
            continue
        for pattern in patterns:
            for path in sorted(base.rglob(pattern)):
                if EXCLUDED_PARTS.intersection(path.parts):
                    continue
                sources[path.relative_to(root).as_posix()] = path.read_text(
                    encoding="utf-8", errors="ignore"
                )
    return sources


def _gate_texts(root: Path) -> dict[str, str]:
    return {
        relative: (root / relative).read_text(encoding="utf-8")
        for relative, _ in SUBSTRATE_GATES
        if (root / relative).is_file()
    }


def gate_declaration_errors(gates: dict[str, str]) -> list[str]:
    """A gate declared "refuses-skip" must actually refuse a skip.

    Without this the coverage claim below is a comment: someone deletes the
    refusal, every test that gate runs becomes a silent skip again, and this
    checker keeps reporting them covered.
    """

    errors = []
    for relative, coverage in SUBSTRATE_GATES:
        text = gates.get(relative)
        if text is None:
            errors.append(
                f"{relative}: SUBSTRATE_GATES names a gate script that is not here; the coverage "
                "it was claimed to provide no longer exists"
            )
        elif coverage == "refuses-skip" and not SKIP_REFUSAL.search(text):
            errors.append(
                f"{relative}: is declared as the gate that refuses to skip, but its text no "
                "longer fails when its skip counter is nonzero; every test it covers is a "
                "silent skip again"
            )
    return errors


def covered_crates(gates: dict[str, str]) -> tuple[set[str], set[str]]:
    """(crates a gate runs, crates a gate runs under a mode that refuses skips)."""

    covered: set[str] = set()
    unskippable: set[str] = set()
    for relative, coverage in SUBSTRATE_GATES:
        crates = set(GATE_ENTRY.findall(gates.get(relative, "")))
        covered |= crates
        if coverage == "refuses-skip":
            unskippable |= crates
    return covered, unskippable


def _names(match: "re.Match[str]") -> str:
    return next(group for group in match.groups() if group)


def supplied_environment(sources: dict[str, str], gates: dict[str, str]) -> set[str]:
    """Every AWAKEN_* variable something in this repository sets.

    A gate script, an e2e runner spawning a child, a workflow: whoever sets it,
    it is supplied. Nothing else may be read as a substrate switch.
    """

    supplied: set[str] = set()
    for relative, body in sources.items():
        if relative.endswith(".rs"):
            continue
        supplied.update(match.group(1) for match in ENVIRONMENT_SUPPLY.finditer(body))
    for text in gates.values():
        supplied.update(match.group(1) for match in ENVIRONMENT_SUPPLY.finditer(text))
    return supplied


def unsupplied_switch_violations(
    sources: dict[str, str], gates: dict[str, str]
) -> list[str]:
    """R3: the variable a skip names must be one something supplies.

    The scope is deliberately the *skip site*, not every environment read: a
    production configuration knob is not a substrate switch. What is refused is
    the shape of the live defect — a short-circuit that names the variable it
    wanted, where that spelling is one nothing in this repository ever sets. That
    test does not skip sometimes; it skips always, and says so in a line nobody
    reads.
    """

    supplied = supplied_environment(sources, gates)
    errors: list[str] = []
    seen: set[tuple[str, str]] = set()
    for relpath, text in sources.items():
        if relpath.startswith("scripts/"):
            continue  # a gate script is the supplier, not a reader
        lines = _executable_lines(text)
        for index, line in enumerate(lines):
            if not SKIP.search(line):
                continue
            window = "\n".join(lines[max(index - 2, 0) : index + 3])
            for name in set(SWITCH_NAME.findall(window)):
                if name in supplied or (relpath, name) in seen:
                    continue
                seen.add((relpath, name))
                if relpath in SILENT_SKIP_EXEMPTIONS:
                    continue
                errors.append(
                    f"{relpath}:{index + 1}: skips and names {name}, but nothing in this "
                    "repository sets that variable, so this skip is unconditional and has never "
                    "not happened. Read the spelling the gate that stands the substrate up "
                    "exports, make a gate export this one, or record in SILENT_SKIP_EXEMPTIONS "
                    "why never running is sound"
                )
    return errors


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


def silent_skip_violations(sources: dict[str, str], gates: dict[str, str]) -> list[str]:
    covered, unskippable = covered_crates(gates)
    supplied_vars = {
        match.group(1) for text in gates.values() for match in ENVIRONMENT_SUPPLY.finditer(text)
    }
    errors = []
    seen: set[str] = set()
    for relpath, text in sources.items():
        lines = _executable_lines(text)
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
            # Recorded before the coverage decision: `seen` answers "does this
            # file still short-circuit", which is what makes a stale registry
            # entry detectable. A covered short-circuit is still a short-circuit.
            seen.add(relpath)
            supplied = crate is not None and crate in covered and any(
                var in text for var in supplied_vars
            )
            # A gate that fails on any skip covers everything it runs outright:
            # there, the skip is a failure, so it cannot be silent.
            if crate is not None and crate in unskippable:
                supplied = True
            if supplied or relpath in SILENT_SKIP_EXEMPTIONS:
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
    sources = _sources(root)
    gates = _gate_texts(root)
    rust_sources = {
        relative: text for relative, text in sources.items() if relative.endswith(".rs")
    }
    return (
        gate_declaration_errors(gates)
        + pg_gate_violations(rust_sources, gates.get(PG_GATE, ""))
        + silent_skip_violations(sources, gates)
        + unsupplied_switch_violations(sources, gates)
    )


def selftest() -> None:
    """Decision table, one row per shape the survey of this repository found.

    R1, over a crate whose tests short-circuit without a database:
      C1 it is named by the Postgres gate        => admitted
      C2 it is not                               => rejected
      C3 it is not, but is exempt                => admitted
      C4 an exemption names a crate that no longer qualifies => rejected

    R2, over one short-circuit:
      C5 registered                              => admitted
      C6 not registered                          => rejected
      C7 the crate is gated and the gate exports what it waits for => admitted
      C8 the crate is gated but waits for something the gate does not supply
                                                 => rejected
      C9 the crate is run by a gate that fails on any skip => admitted
      C10 the skip spelling is `skip:` (the live instance), `[SKIP]`, `skipped:`
          or bare `skipping`                     => rejected, every one of them
      C11 the "skip" is in a comment             => admitted; prose is not a skip
      C12 the file is outside crates/** (e2e/**, scripts/**) => still scanned
      C17 an exemption names an explicitly ignored test with no successful skip => rejected
      C18 the same ignored test without an exemption => admitted; missing credentials fail when selected
      C19 the canonical required executor selects the current crate and supplies its switch => admitted
      C20 the same command selects the old crate instead => rejected
      C21 the current crate has no supplied dependency switch => rejected
      C22 an unrecognized wrapper cannot claim to run the current crate => rejected

    R3, over the variable a skip names:
      C13 something in the repository sets it    => admitted
      C14 nothing does                           => rejected (the live instance)

    Gate declarations:
      C15 a gate declared "refuses-skip" still refuses => admitted
      C16 it no longer does                            => rejected
    """

    global PG_GATE_EXEMPTIONS, SILENT_SKIP_EXEMPTIONS
    original_pg, original_skip = PG_GATE_EXEMPTIONS, SILENT_SKIP_EXEMPTIONS
    # The synthetic table must not inherit the real registry, or a live
    # exemption naming a crate absent from the fixture reads as a stale entry.
    PG_GATE_EXEMPTIONS = {}
    SILENT_SKIP_EXEMPTIONS = {}
    try:
        gated = "pg_test cargo test -p awaken-thing --lib\n"
        src = {
            "crates/stores/awaken-thing/src/lib.rs":
                'let url = std::env::var("AWAKEN_TEST_DATABASE_URL");'
        }
        assert not pg_gate_violations(src, gated), "C1/E1"
        assert pg_gate_violations(src, "pg_test cargo test -p other\n"), "C2/E2"
        PG_GATE_EXEMPTIONS = {"awaken-thing": "test"}
        assert not pg_gate_violations(src, "pg_test cargo test -p other\n"), "C3/E1"
        PG_GATE_EXEMPTIONS = {"awaken-ghost": "test"}
        assert pg_gate_violations(src, "pg_test cargo test -p other\n"), "C4/E2"
        PG_GATE_EXEMPTIONS = {}

        supplying = {PG_GATE: "export AWAKEN_TEST_DATABASE_URL=x\npg_test cargo test -p awaken-thing"}
        empty = {PG_GATE: ""}

        skipping = {"crates/a/b/src/x.rs": 'println!("[skip] no Postgres reachable");\n    return;'}
        assert silent_skip_violations(skipping, empty), "C6/E2"
        SILENT_SKIP_EXEMPTIONS = {"crates/a/b/src/x.rs": "test"}
        assert not silent_skip_violations(skipping, empty), "C5/E1"
        ignored = {"crates/a/b/src/x.rs": (
            '#[test]\n#[ignore = "requires live credentials"]\n'
            'fn live() { std::env::var("AWAKEN_LIVE_TOKEN").expect("credential required"); }\n'
        )}
        assert silent_skip_violations(ignored, empty), "C17/E2 stale successful-skip exemption"
        SILENT_SKIP_EXEMPTIONS = {}
        assert not silent_skip_violations(ignored, empty), "C18/E1 explicit ignored qualification"

        assert not silent_skip_violations(
            {"crates/a/awaken-thing/src/x.rs":
                'AWAKEN_TEST_DATABASE_URL\nprintln!("[skip] no Postgres reachable");\n    return;'},
            supplying,
        ), "C7/E1 a gated crate needs no entry for a dependency the gate supplies"
        assert silent_skip_violations(
            {"crates/a/awaken-thing/src/x.rs": 'no NATS reachable; skipping\n    return;'},
            supplying,
        ), "C8/E2 a gated crate still owes an entry for a dependency it does not supply"

        # Migration decision table C19-C22 crosses the selected owner, the
        # supplied live dependency and the executor identity. Only the existing
        # canonical executor plus the current owner/switch covers this skip.
        # Its own execution tests separately refuse zero selection, a skipped
        # outcome and failed assertions; this reader does not reimplement them.
        pvc = {"crates/server/awaken-session-realization/src/cache_volume.rs":
               'AWAKEN_K8S_E2E\neprintln!("skipping: no live PVC");\nreturn;'}
        pvc_gate = (
            'AWAKEN_K8S_E2E=1 run_required_rust_tests -p awaken-session-realization '
            '--features k8s-tests --lib --locked\n'
        )
        k8s_gate = "scripts/e2e/k8s_container_e2e.sh"
        assert not silent_skip_violations(pvc, {k8s_gate: pvc_gate}), "C19/E1"
        assert silent_skip_violations(pvc, {k8s_gate: pvc_gate.replace(
            "-p awaken-session-realization", "-p awaken-session-executor")}), "C20/E2 stale owner"
        assert silent_skip_violations(pvc, {k8s_gate: pvc_gate.replace(
            "AWAKEN_K8S_E2E=1 ", "")}), "C21/E2 missing live dependency"
        for other in ("unqualified_test_wrapper", "unqualified_run_required_rust_tests",
                      "run_required_rust_tests_unqualified"):
            assert silent_skip_violations(pvc, {k8s_gate: pvc_gate.replace(
                "run_required_rust_tests", other)}), f"C22/E2 unknown executor {other}"

        refusing = {
            "scripts/e2e/sandbox_capability_suite.sh":
                'cargo test -q -p awaken-thing --lib\n'
                'if [ "$require_all" -eq 1 ] && [ "$SKIP" -ne 0 ]; then exit 1; fi\n'
        }
        assert not silent_skip_violations(
            {"crates/a/awaken-thing/src/x.rs": 'println!("SKIP: no bwrap");\n    return;'},
            refusing,
        ), "C9/E1 a gate that fails on any skip covers what it runs"

        for spelling in ("[skip] gone", "SKIP: gone", "skipped: gone", "no broker; skipping"):
            assert silent_skip_violations(
                {"crates/a/b/src/x.rs": f'println!("{spelling}");\n    return;'}, empty
            ), f"C10/E2 {spelling!r} is a skip"
        assert not silent_skip_violations(
            {"crates/a/b/src/x.rs": '// SKIP: this comment describes the rule\n    return;'}, empty
        ), "C11/E1 a comment is prose, not a short-circuit"
        assert silent_skip_violations(
            {"e2e/thing_e2e.mjs": "console.log('SKIP: no engine');\n  process.exit(0);"}, empty
        ), "C12/E2 the corpus reaches beyond crates/**"

        supplied_switch = {
            "crates/a/b/tests/x.rs": 'eprintln!("skip: AWAKEN_TEST_DATABASE_URL unset");\n    return;'
        }
        assert not unsupplied_switch_violations(supplied_switch, supplying), "C13/E1"
        unsupplied_switch = {
            "crates/a/b/tests/x.rs": 'eprintln!("skip: AWAKEN_TEST_PG_URL unset");\n    return;'
        }
        assert unsupplied_switch_violations(unsupplied_switch, supplying), \
            "C14/E2 the reported defect: the gate exported one spelling, the test read another"

        assert not gate_declaration_errors({
            PG_GATE: "", "scripts/e2e/k8s_container_e2e.sh": "",
            "scripts/e2e/sandbox_capability_suite.sh":
                'if [ "$require_all" -eq 1 ] && [ "$SKIP" -ne 0 ]; then exit 1; fi',
        }), "C15/E1"
        assert gate_declaration_errors({
            PG_GATE: "", "scripts/e2e/k8s_container_e2e.sh": "",
            "scripts/e2e/sandbox_capability_suite.sh": "echo done",
        }), "C16/E2 a gate that stopped refusing skips stops covering anything"
    finally:
        PG_GATE_EXEMPTIONS, SILENT_SKIP_EXEMPTIONS = original_pg, original_skip
    print("check_dependency_gates selftest: the decision table holds")


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for: that is how
# `check-all.sh` invoked `--selftest` for years while the flag was `--self-test`
# and the self-test never once executed. ``allow_abbrev=False`` closes the
# second half: without it ``--self-tes`` is also accepted, so the flag has no
# single spelling.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_dependency_gates.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run the decision table instead of the scan"
    )
    return parser.parse_args(argv)


if __name__ == "__main__":
    if parse(sys.argv[1:]).self_test:
        selftest()
        raise SystemExit(0)
    problems = check_all(ROOT)
    for problem in problems:
        print(f"ERROR: {problem}", file=sys.stderr)
    if problems:
        raise SystemExit(1)
    print("OK - every dependency gate fails rather than skips.")
