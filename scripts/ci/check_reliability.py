#!/usr/bin/env python3
"""Validate the one reliability evidence path and operational objectives."""

from __future__ import annotations

import argparse
import json
import os
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
ASSUMPTIONS = ROOT / "formal" / "assumptions.json"
RELIABILITY_SCRIPT = ROOT / "scripts" / "ci" / "check-reliability.sh"

REQUIRED_SURFACES = (
    "crates/devtools/awaken-reliability-testkit/src/lib.rs",
    "crates/server/awaken-run-ingress-testkit/src/conformance/reliability.rs",
    "crates/stores/awaken-store-fs/tests/recovery.rs",
    "crates/stores/awaken-store-sqlite/tests/failure_atomicity.rs",
    "crates/stores/awaken-session-store/tests/process_crash_outbox.rs",
    "crates/stores/awaken-credential-store/tests/process_crash_creation.rs",
    "crates/control/awaken-config-store/tests/process_crash_audit.rs",
    "e2e/managed_session_config_restart_e2e.mjs",
    "e2e/durable_soak_fairness_e2e.mjs",
    "fuzz/fuzz_targets/sse_chunks.rs",
    "fuzz/fuzz_targets/thread_commit_wire.rs",
)

REQUIRED_TOOL_GATES = (
    "cargo +nightly fuzz run sse_chunks",
    "cargo +nightly fuzz run thread_commit_wire",
    "cargo +nightly miri test -p awaken-mcp-wire --lib",
    "-Zsanitizer=address",
    "-Zsanitizer=thread",
    "cargo mutants",
    "cleanup_mutation_artifacts",
    "cleanup_mutation_artifacts\npython3 scripts/ci/check_reliability.py",
)


def validation_errors(
    assumptions: dict[str, object],
    script: str,
    existing: set[str],
    crash_sources: dict[str, str],
) -> list[str]:
    errors: list[str] = []
    missing = sorted(set(REQUIRED_SURFACES) - existing)
    if missing:
        errors.append("missing reliability surfaces: " + ", ".join(missing))
    for marker in REQUIRED_TOOL_GATES:
        if marker not in script:
            errors.append(f"reliability gate missing {marker!r}")

    rows = assumptions.get("assumptions")
    if not isinstance(rows, list):
        return errors + ["formal assumptions are not a list"]
    for row in rows:
        if not isinstance(row, dict) or row.get("kind") == "semantic":
            continue
        objective = row.get("reliability_objective")
        if not isinstance(objective, dict):
            errors.append(f"{row.get('id')} has no reliability_objective")
            continue
        target = objective.get("target")
        if not isinstance(target, (int, float)) or not 0 < target <= 1:
            errors.append(f"{row.get('id')} has invalid objective target")
        for field in ("indicator", "window_days", "max_burn_rate", "scope"):
            if not objective.get(field):
                errors.append(f"{row.get('id')} objective missing {field}")
        for field in ("recovery_time_seconds", "recovery_point_seconds"):
            value = objective.get(field)
            if not isinstance(value, int) or value < 0:
                errors.append(f"{row.get('id')} objective has invalid {field}")
        drills = objective.get("drill_evidence")
        if not isinstance(drills, list) or not drills:
            errors.append(f"{row.get('id')} objective has no drill_evidence")
        else:
            for evidence in drills:
                if evidence not in existing:
                    errors.append(f"{row.get('id')} drill evidence missing: {evidence}")

    # The shared CrashProcess fixture is the sole child-kill orchestration
    # owner. Domain tests may define their child behavior, but must not copy the
    # parent-side polling/spawn implementation.
    for path, source in crash_sources.items():
        if "Command::new(std::env::current_exe" in source:
            errors.append(f"{path} duplicates the CrashProcess harness")
    return errors


def current_inputs() -> tuple[dict[str, object], str, set[str], dict[str, str]]:
    existing: set[str] = set()
    for directory, child_dirs, files in os.walk(ROOT):
        child_dirs[:] = [
            name
            for name in child_dirs
            if name not in {"target", ".git"} and not name.startswith("mutants.out")
        ]
        parent = Path(directory)
        existing.update((parent / name).relative_to(ROOT).as_posix() for name in files)
    crash_sources = {
        relative: (ROOT / relative).read_text(encoding="utf-8")
        for relative in existing
        if Path(relative).name.startswith("process_crash")
        or Path(relative).name == "failure_atomicity.rs"
    }
    return (
        json.loads(ASSUMPTIONS.read_text(encoding="utf-8")),
        RELIABILITY_SCRIPT.read_text(encoding="utf-8"),
        existing,
        crash_sources,
    )


class ReliabilityCheckerTests(unittest.TestCase):
    def test_decision_table_rejects_each_parallel_or_incomplete_path(self) -> None:
        # Cause/effect graph: C1 every surface/tool gate exists; C2 every
        # non-semantic external assumption has a bounded objective and drill;
        # C3 crash tests delegate to CrashProcess; C4 mutation artifacts are
        # cleaned after success, failure, or interruption. Effects: E1 accept
        # all; E2 missing gate rejects; E3 missing objective rejects; E4
        # duplicate process harness rejects; E5 missing mutation cleanup
        # rejects. Rules R1=C1+C2+C3+C4; R2=!C1; R3=!C2; R4=!C3; R5=!C4.
        inputs = current_inputs()
        self.assertEqual(validation_errors(*inputs), [], "R1")

        script_mutant = inputs[1].replace("cargo +nightly fuzz run sse_chunks", "cargo test")
        self.assertTrue(validation_errors(inputs[0], script_mutant, inputs[2], inputs[3]), "R2")

        assumptions_mutant = json.loads(json.dumps(inputs[0]))
        next(
            row for row in assumptions_mutant["assumptions"] if row["kind"] != "semantic"
        ).pop("reliability_objective")
        self.assertTrue(
            validation_errors(assumptions_mutant, inputs[1], inputs[2], inputs[3]), "R3"
        )

        crash_mutant = dict(inputs[3])
        crash_mutant["process_crash_mutant.rs"] = "Command::new(std::env::current_exe())"
        self.assertTrue(
            validation_errors(inputs[0], inputs[1], inputs[2], crash_mutant), "R4"
        )

        cleanup_mutant = inputs[1].replace(
            "cleanup_mutation_artifacts", "mutation_artifacts_are_not_cleaned"
        )
        self.assertTrue(
            validation_errors(inputs[0], cleanup_mutant, inputs[2], inputs[3]), "R5"
        )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ReliabilityCheckerTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    errors = validation_errors(*current_inputs())
    if errors:
        print("\n".join(f"ERROR: {error}" for error in errors), file=sys.stderr)
        return 1
    print("reliability evidence: objectives, drills, tools, and crash owner are complete")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
