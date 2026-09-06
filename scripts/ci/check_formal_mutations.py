#!/usr/bin/env python3
"""Mutation-test the formal evidence and residual-boundary gates.

Metadata mutants prove that the repository gate also
rejects a proof removed from CI, a downgraded proof claim, and incomplete or
invented residual-boundary documentation. The opt-in Memory copy mutant uses
the pinned TLC supplied by the strict gate to check the recovery-base invariant.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]


def prepare_case(case_root: Path) -> None:
    # Copy every writable mutation target and its checker dependencies. The old
    # fixture omitted formal/tla and Web classifications, so an unmutated case
    # already failed and any nonzero-exit oracle reported false kills.
    ignore = shutil.ignore_patterns("__pycache__", ".tlacache")
    shutil.copytree(ROOT / "scripts/ci", case_root / "scripts/ci", ignore=ignore)
    shutil.copytree(ROOT / "formal", case_root / "formal", ignore=ignore)
    for source in (ROOT / "scripts").iterdir():
        if source.name != "ci":
            os.symlink(source, case_root / "scripts" / source.name, target_is_directory=source.is_dir())
    for source in ROOT.iterdir():
        if source.name not in {".git", "scripts", "formal", "target", "node_modules"}:
            os.symlink(source, case_root / source.name, target_is_directory=source.is_dir())


def run_checker(case_root: Path, checker: str, *arguments: str) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(
            [sys.executable, str(case_root / "scripts/ci" / checker), *arguments],
            cwd=case_root, text=True, capture_output=True, check=False, timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise SystemExit(f"formal mutation infrastructure failure: {checker}: {error}") from error


def must_fail(case_root: Path, checker: str, label: str, *arguments: str, expected: str) -> None:
    result = run_checker(case_root, checker, *arguments)
    if result.returncode == 0:
        raise SystemExit(f"formal mutation survived: {label}")
    if result.returncode != 1 or "Traceback" in result.stderr or expected not in result.stderr:
        raise SystemExit(
            f"formal mutation wrong failure: {label}; expected {expected!r}; "
            f"exit={result.returncode}; stderr={result.stderr}"
        )


def require_green_baseline(case_root: Path) -> None:
    for checker, args in (
        ("check_formal_coverage.py", ("--require-evidence-complete",)),
        ("check_feature_coverage.py", ("--require-complete",)),
        ("check_proof_boundaries.py", ()),
    ):
        result = run_checker(case_root, checker, *args)
        if result.returncode != 0:
            raise SystemExit(f"formal mutation baseline failed: {checker}: {result.stderr}")


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def mutate_removed_harness(case_root: Path) -> None:
    ledger = json.loads((case_root / "formal" / "coverage.json").read_text())
    harness = next(row["proof_harnesses"][0] for row in ledger["obligations"] if row.get("proof_harnesses"))
    gate = case_root / "scripts" / "ci" / "check_formal.sh"
    source = gate.read_text()
    needle = f"--harness {harness}"
    if needle not in source:
        raise SystemExit(f"mutation fixture missing strict-CI harness {harness}")
    gate.write_text(source.replace(needle, f"--harness removed_{harness}", 1))
    must_fail(case_root, "check_formal_coverage.py", "removed Kani harness from strict CI",
              expected=f"Kani harness {harness} is absent from strict CI")


def mutate_downgraded_claim(case_root: Path) -> None:
    path = case_root / "formal" / "coverage.json"
    ledger = json.loads(path.read_text())
    row = next(item for item in ledger["obligations"] if item["status"] == "kernel_proved")
    row["status"] = "executable_only"
    row.pop("proof_harnesses", None)
    write_json(path, ledger)
    must_fail(case_root, "check_formal_coverage.py", "downgraded proved obligation",
              expected=f"{row['id']} production-kernel grade has no Kani harness")


def mutate_commented_harness(case_root: Path) -> None:
    # H1: exact source proof and ledger survive, but its runner argument becomes
    # a shell comment. Text presence is not execution selection; reject it.
    gate = case_root / "scripts/ci/check_formal.sh"
    source = gate.read_text()
    harness = "work_lease_epoch_is_exact_nonnegative_and_nonwrapping"
    source = source.replace(f"--harness {harness}", f"--harness removed_{harness}", 1)
    gate.write_text(source + f"\n# --harness {harness}\n")
    must_fail(case_root, "check_formal_coverage.py", "comment-only Kani selection",
              expected=f"Kani harness {harness} is absent from strict CI")


def mutate_wrong_harness_package(case_root: Path) -> None:
    # H2: unchanged exact harness name under a different package must not earn
    # proof credit. Package ownership comes from Cargo, not directory spelling.
    gate = case_root / "scripts/ci/check_formal.sh"
    gate.write_text(gate.read_text().replace("run_kani awaken-work-store", "run_kani awaken-config-resolver", 1))
    must_fail(case_root, "check_formal_coverage.py", "Kani selection in wrong package",
              expected="Kani harness work_lease_epoch_is_exact_nonnegative_and_nonwrapping is selected from wrong package")


def mutate_harness_name_prefix(case_root: Path) -> None:
    # H3: adding a short name that is merely a prefix of a real proof, plus a
    # comment mentioning that name, cannot manufacture a proof definition.
    path = case_root / "formal/coverage.json"
    ledger = json.loads(path.read_text())
    row = next(item for item in ledger["obligations"] if item["id"] == "work.persisted_epoch_is_exact_nonnegative_and_nonwrapping")
    row["proof_harnesses"].append("work_lease_epoch")
    write_json(path, ledger)
    gate = case_root / "scripts/ci/check_formal.sh"
    gate.write_text(gate.read_text() + "\n# --harness work_lease_epoch\n")
    must_fail(case_root, "check_formal_coverage.py", "prefix is not a proof definition",
              expected="Kani harness work_lease_epoch is not a unique proof definition in its evidence")


def mutate_duplicate_harness_selection(case_root: Path) -> None:
    # H4: repeating one proof invocation cannot satisfy a second proof identity
    # or obscure an ambiguous runner binding. One source has one exact selection.
    gate = case_root / "scripts/ci/check_formal.sh"
    gate.write_text(gate.read_text() + "\nrun_kani awaken-work-store --harness work_lease_epoch_is_exact_nonnegative_and_nonwrapping\n")
    must_fail(case_root, "check_formal_coverage.py", "duplicate Kani selection",
              expected="Kani harness work_lease_epoch_is_exact_nonnegative_and_nonwrapping is selected from wrong package or more than once")


def mutate_unchecked_model(case_root: Path) -> None:
    gate = case_root / "scripts" / "ci" / "check_formal.sh"
    source = gate.read_text()
    ledger = json.loads((case_root / "formal" / "coverage.json").read_text())
    model = next(
        evidence
        for row in ledger["obligations"]
        for evidence in row["evidence"]
        if evidence.startswith("formal/tla/") and evidence.endswith(".tla") and evidence in source
    )
    gate.write_text(source.replace(model, "formal/tla/RemovedModel.tla"))
    must_fail(case_root, "check_formal_coverage.py", "formal model removed from strict CI",
              expected=f"formal evidence {model} is absent from strict CI")


def mutate_commented_release_self_test(case_root: Path) -> None:
    # P1: the Python authority and model still exist, but its only executable
    # strict-CI selection is commented out. Static text presence earns no link.
    gate = case_root / "scripts/ci/check_formal.sh"
    source = gate.read_text()
    anchor = "python3 scripts/release/verification_manifest.py self-test"
    if source.count(anchor) != 1:
        raise SystemExit("release self-test mutation anchor drifted")
    gate.write_text(source.replace(anchor, "# " + anchor))
    must_fail(case_root, "check_formal_coverage.py", "comment-only Python self-test",
              expected="assembly_role.verified_admission_requires_exact_fresh_tcb_fingerprint is model-linked without executable production evidence")


def mutate_missing_proof_grade(case_root: Path) -> None:
    path = case_root / "formal" / "coverage.json"
    ledger = json.loads(path.read_text())
    ledger["obligations"][0].pop("proof_grade")
    write_json(path, ledger)
    must_fail(case_root, "check_formal_coverage.py", "proof grade removed",
              expected=f"{ledger['obligations'][0]['id']} has unknown or missing proof_grade None")


def mutate_false_production_grade(case_root: Path) -> None:
    path = case_root / "formal" / "coverage.json"
    ledger = json.loads(path.read_text())
    row = next(
        item
        for item in ledger["obligations"]
        if item["proof_grade"] == "abstract_model_checked"
        and not item.get("proof_harnesses")
    )
    row["proof_grade"] = "production_kernel_proved"
    write_json(path, ledger)
    must_fail(
        case_root,
        "check_formal_coverage.py",
        "abstract model relabeled as production proof",
        expected=f"{row['id']} production-kernel grade has no Kani harness",
    )


def mutate_missing_boundary(case_root: Path) -> None:
    path = case_root / "formal" / "proof-boundaries.json"
    ledger = json.loads(path.read_text())
    removed = ledger["boundaries"].pop()
    write_json(path, ledger)
    must_fail(case_root, "check_proof_boundaries.py", "missing residual boundary",
              expected=f"proof boundaries: missing ['{removed['requirement_id']}']")


def mutate_false_refinement_grade(case_root: Path) -> None:
    path = case_root / "formal/coverage.json"
    ledger = json.loads(path.read_text())
    row = next(item for item in ledger["obligations"] if
               "crates/runtime/awaken-runtime/tests/formal_refinement.rs" in item["evidence"])
    row["proof_grade"] = "implementation_refined"
    write_json(path, ledger)
    must_fail(case_root, "check_formal_coverage.py", "trace relabeled as universal refinement",
              expected=f"{row['id']} implementation_refined requires a checked universal production refinement")


def mutate_invented_boundary(case_root: Path) -> None:
    path = case_root / "formal" / "proof-boundaries.json"
    ledger = json.loads(path.read_text())
    mutant = dict(ledger["boundaries"][0])
    mutant["requirement_id"] = "invented.requirement.boundary"
    ledger["boundaries"].append(mutant)
    write_json(path, ledger)
    must_fail(case_root, "check_proof_boundaries.py", "invented residual boundary",
              expected="proof boundaries: extra ['invented.requirement.boundary']")


def mutate_unlinked_product_obligation(case_root: Path) -> None:
    path = case_root / "formal" / "features.json"
    ledger = json.loads(path.read_text())
    target = "sandbox_control.single_publication_is_stale_fenced_non_wrapping_and_close_absorbing"
    owner = next(
        requirement
        for feature in ledger["features"]
        for requirement in feature["requirements"]
        if target in requirement.get("obligations", [])
    )
    owner["obligations"].remove(target)
    write_json(path, ledger)
    must_fail(
        case_root,
        "check_feature_coverage.py",
        "formal obligation detached from its product requirement",
        "--require-complete",
        expected=f"formal obligations are disconnected from every product journey: {target}",
    )


def mutate_reopened_external_assumption(case_root: Path) -> None:
    path = case_root / "formal" / "assumptions.json"
    ledger = json.loads(path.read_text())
    ledger["assumptions"][0]["status"] = "open"
    write_json(path, ledger)
    must_fail(
        case_root,
        "check_feature_coverage.py",
        "evidenced external assumption reopened",
        "--require-complete",
        expected="completion required but gaps remain: requirements=0, assumptions=1",
    )


def mutate_missing_tcb_contract(case_root: Path) -> None:
    path = case_root / "formal" / "assumptions.json"
    ledger = json.loads(path.read_text())
    ledger["assumptions"][0].pop("fail_closed_behavior")
    write_json(path, ledger)
    must_fail(
        case_root,
        "check_feature_coverage.py",
        "external assumption lost fail-closed contract",
        "--require-complete",
        expected=f"{ledger['assumptions'][0]['id']} has no fail_closed_behavior",
    )


def mutate_disconnected_product_feature(case_root: Path) -> None:
    path = case_root / "formal" / "features.json"
    ledger = json.loads(path.read_text())
    # This feature is not a journey's sole formal anchor owner: removing it
    # isolates feature disconnection instead of failing anchor admission first.
    target = "runtime.compaction"
    removed = False
    for journey in ledger["product_journeys"]:
        if target in journey["stages"]:
            journey["stages"].remove(target)
            removed = True
    if not removed:
        raise SystemExit(f"mutation fixture has no product journey stage {target}")
    write_json(path, ledger)
    must_fail(
        case_root,
        "check_feature_coverage.py",
        "product feature disconnected from every assurance journey",
        "--require-complete",
        expected=f"product features are disconnected from every assurance journey: {target}",
    )


class MutationOracleTests(unittest.TestCase):
    def test_constant_trace_refutation_requires_exact_name_and_status(self) -> None:
        # Q1 TLC rejects a false constant invariant before state exploration:
        # exact 151+named false relation is a kill. Q2 unrelated syntax/tool
        # failures, another relation or a successful exit are never kills.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        for code, output, accepted in [(151, "The invariant of TraceRefines is equal to FALSE", True),
            (151, "The invariant of Other is equal to FALSE", False), (151, "parse error", False),
            (0, "The invariant of TraceRefines is equal to FALSE", False)]:
            with self.subTest(code=code, output=output), patch("subprocess.run", side_effect=[green, subprocess.CompletedProcess([], code, output)]):
                if accepted:
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config", [("mutant", "config", "constant:TraceRefines")])
                else:
                    with self.assertRaises(SystemExit):
                        check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config", [("mutant", "config", "constant:TraceRefines")])

    def test_observation_time_embedding_preserves_and_reflects_relations(self) -> None:
        from render_runtime_refinement_traces import normalize_observation_times
        # Q4 TLC's integer bound must not truncate, saturate or conflate actual
        # clocks. Equal/order/max outcomes must agree for every pair, including
        # legacy None, zero, i32 overflow and u64::MAX. Non-time cursors stay exact.
        values = [0, 1, 2**31, 1788710583713, 2**64 - 1]
        steps = [{"before": {"time": a, "sequence": 7}, "after": {"time": b}, "now": max(a, b)} for a in values for b in values]
        mapped = normalize_observation_times(steps)
        for original, projected in zip(steps, mapped):
            a, b = original["before"]["time"], original["after"]["time"]
            x, y = projected["before"]["time"], projected["after"]["time"]
            self.assertEqual(a == b, x == y)
            self.assertEqual(a < b, x < y)
            self.assertEqual(max(x, y), projected["now"])
            self.assertEqual(projected["before"]["sequence"], 7)
        self.assertEqual(normalize_observation_times([{"time": None}]), [{"time": None}])
        for invalid in [-1, True, "100"]:
            with self.assertRaises(ValueError):
                normalize_observation_times([{"time": invalid}])

    def test_observation_trace_gate_rejects_missing_scenarios_and_schema_drift(self) -> None:
        from render_runtime_refinement_traces import render_observations
        manifest = json.loads((ROOT / "formal/runtime-refinement-manifest.json").read_text())
        # Q3 fresh empty output, unknown fields, or incomplete production
        # scenarios cannot retain green coverage from a prior invocation.
        with tempfile.TemporaryDirectory() as temp:
            raw, output = Path(temp) / "raw", Path(temp) / "rendered"
            raw.mkdir()
            with self.assertRaises(ValueError):
                render_observations(raw, output, ROOT, manifest, True)
            write_json(raw / "bad.json", {"schema_version": 1})
            with self.assertRaises(ValueError):
                render_observations(raw, output, ROOT, manifest, False)
            (raw / "bad.json").unlink()
            # Q5 a present name with another family's relation cannot count as
            # the required producer. Q6 a valid partial run must still fail the
            # complete gate, even when the output directory already exists.
            document = {"schema_version": 3, "projection_id": "session-observations-v3", "name": "binding-apply", "family": "processed", "steps": [{}]}
            write_json(raw / "binding-apply.json", document)
            with self.assertRaisesRegex(ValueError, "scenario/family drift"):
                render_observations(raw, output, ROOT, manifest, False)
            document["family"] = "root"
            document["steps"] = [{"rule": "binding-apply", "before": {"exists": True, "revision": 1, "created": 100, "updated": 100}, "after": {"exists": True, "revision": 2, "created": 100, "updated": 200}, "now": 200}]
            write_json(raw / "binding-apply.json", document)
            with self.assertRaisesRegex(ValueError, "coverage is incomplete"):
                render_observations(raw, output, ROOT, manifest, True)

    def test_temporal_mutation_requires_exact_exit_and_a_single_property(self) -> None:
        # T1 green base + exit 13 + temporal violation + one named property is
        # a kill. T2 unrelated safety/parser/tool failures cannot count. T3 a
        # different/multiple property selection cannot use TLC's generic text.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        for config, code, output, accepted in (
            ("PROPERTIES Releases", 13, "Temporal properties were violated.", True),
            ("PROPERTIES Releases", 12, "Invariant Safety is violated", False),
            ("PROPERTIES Releases", 1, "parse error", False),
            ("PROPERTIES Releases", 0, "Temporal properties were violated.", False),
            ("PROPERTIES Releases Other", 13, "Temporal properties were violated.", False),
            ("PROPERTIES Other", 13, "Temporal properties were violated.", False),
        ):
            with self.subTest(config=config, code=code), patch("subprocess.run", side_effect=[
                green, subprocess.CompletedProcess([], code, output),
            ]):
                if accepted:
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", config,
                                              [("mutant", config, "temporal:Releases")])
                else:
                    with self.assertRaises(SystemExit):
                        check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", config,
                                                  [("mutant", config, "temporal:Releases")])

    def test_action_mutation_requires_exact_named_property_and_exit(self) -> None:
        # A1 exact named action violation + exit 13 => kill; A2 wrong name,
        # generic temporal text, parser error or successful exit => not a kill.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        for code, output, accepted in [(13, "Action property Stable is violated.", True),
            (13, "Action property Other is violated.", False), (13, "Temporal properties were violated.", False),
            (1, "parse error", False), (0, "Action property Stable is violated.", False)]:
            with self.subTest(code=code, output=output), patch("subprocess.run", side_effect=[green, subprocess.CompletedProcess([], code, output)]):
                if accepted:
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "PROPERTY Stable", [("mutant", "PROPERTY Stable", "action:Stable")])
                else:
                    with self.assertRaises(SystemExit):
                        check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "PROPERTY Stable", [("mutant", "PROPERTY Stable", "action:Stable")])

    def test_tlc_mutations_require_a_green_base_and_exact_invariant_failure(self) -> None:
        # TLC cause/effect table: baseline green x mutant exit x invariant text.
        # Only green+12+exact invariant passes. Infrastructure/parser failures,
        # unrelated invariants and surviving mutants must not count as kills.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        for code, output, accepted in (
            (12, "Invariant ExactRule is violated", True),
            (0, "Invariant ExactRule is violated", False),
            (12, "Invariant OtherRule is violated", False),
            (1, "parse error", False),
            (-9, "Invariant ExactRule is violated", False),
        ):
            with self.subTest(code=code, output=output), patch("subprocess.run", side_effect=[
                green, subprocess.CompletedProcess([], code, output),
            ]):
                if accepted:
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config",
                                              [("mutant", "config", "ExactRule")])
                else:
                    with self.assertRaisesRegex(SystemExit, "not killed correctly"):
                        check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config",
                                                  [("mutant", "config", "ExactRule")])
        with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 12,
                "Invariant ExactRule is violated")) as runner:
            with self.assertRaisesRegex(SystemExit, "baseline failed"):
                check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config",
                                          [("mutant", "config", "ExactRule")])
            self.assertEqual(runner.call_count, 1, "a failed base cannot reach its mutant")

    def test_only_the_expected_semantic_rejection_is_a_kill(self) -> None:
        # G1-G5: C1 exit=0/1/signal/parser error; C2 expected rule matches;
        # C3 infrastructure traceback. Only exit=1+C2+!C3 is a killed mutant.
        for code, stderr, killed in (
            (0, "expected rule", False), (1, "expected rule", True),
            (1, "unrelated missing model", False), (-9, "expected rule", False),
            (2, "expected rule", False), (1, "Traceback: expected rule", False),
        ):
            with self.subTest(code=code, stderr=stderr), patch(
                f"{__name__}.run_checker",
                return_value=subprocess.CompletedProcess([], code, "", stderr),
            ):
                if killed:
                    must_fail(ROOT, "checker.py", "oracle", expected="expected rule")
                else:
                    with self.assertRaises(SystemExit):
                        must_fail(ROOT, "checker.py", "oracle", expected="expected rule")

    def test_unavailable_or_timed_out_tool_is_not_a_kill(self) -> None:
        # G5: missing executable and timeout fail before semantic classification.
        for error in (OSError("missing tool"), subprocess.TimeoutExpired("checker", 120)):
            with self.subTest(error=error), patch("subprocess.run", side_effect=error):
                with self.assertRaisesRegex(SystemExit, "infrastructure failure"):
                    must_fail(ROOT, "checker.py", "oracle", expected="expected rule")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    model_selection = parser.add_mutually_exclusive_group()
    model_selection.add_argument("--memory-copy-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--agent-input-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--resource-preparation-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--realization-renewal-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--resolution-failure-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--config-cas-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--background-completion-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--audit-commit-model-mutants", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--effect-retirement-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--credential-removal-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--verified-admission-model-mutants", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--session-network-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--session-observation-model-mutants", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--session-observation-trace-mutants", type=Path, metavar="TLC_JAR")
    parser.add_argument("--observation-trace-dir", type=Path)

    model_selection.add_argument("--cowork-lifecycle-model-mutants", type=Path, metavar="TLC_JAR")
    arguments = parser.parse_args()
    if arguments.session_observation_trace_mutants is not None:
        if arguments.observation_trace_dir is None:
            parser.error("--observation-trace-dir is required")
        check_observation_trace_mutants(arguments.session_observation_trace_mutants, arguments.observation_trace_dir)
        return
    if arguments.session_observation_model_mutants is not None:
        check_session_observation_model_mutants(arguments.session_observation_model_mutants)
        return
    if arguments.cowork_lifecycle_model_mutants is not None:
        check_cowork_lifecycle_model_mutants(arguments.cowork_lifecycle_model_mutants)
        return
    if arguments.realization_renewal_model_mutant is not None:
        check_realization_renewal_model_mutant(arguments.realization_renewal_model_mutant)
        return
    if arguments.session_network_model_mutant is not None:
        check_session_network_model_mutant(arguments.session_network_model_mutant)
        return
    if arguments.verified_admission_model_mutants is not None:
        check_verified_admission_model_mutants(arguments.verified_admission_model_mutants)
        return
    if arguments.credential_removal_model_mutant is not None:
        check_credential_removal_model_mutant(arguments.credential_removal_model_mutant)
        return
    if arguments.memory_copy_model_mutant is not None:
        check_memory_copy_model_mutant(arguments.memory_copy_model_mutant)
        return
    if arguments.agent_input_model_mutant is not None:
        check_agent_input_model_mutant(arguments.agent_input_model_mutant)
        return
    if arguments.resource_preparation_model_mutant is not None:
        check_resource_preparation_model_mutant(arguments.resource_preparation_model_mutant)
        return
    if arguments.resolution_failure_model_mutant is not None:
        check_resolution_failure_model_mutant(arguments.resolution_failure_model_mutant)
        return
    if arguments.config_cas_model_mutant is not None:
        check_config_cas_model_mutant(arguments.config_cas_model_mutant)
        return
    if arguments.background_completion_model_mutant is not None:
        check_background_completion_model_mutant(arguments.background_completion_model_mutant)
        return
    if arguments.audit_commit_model_mutants is not None:
        check_audit_commit_model_mutants(arguments.audit_commit_model_mutants)
        return
    if arguments.effect_retirement_model_mutant is not None:
        check_effect_retirement_model_mutant(arguments.effect_retirement_model_mutant)
        return
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(MutationOracleTests)
    if not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful():
        raise SystemExit(1)
    # Cause/effect decision table: removing a strict-CI proof, downgrading a
    # claim, detaching a model, omitting/inventing a residual boundary,
    # inventing a production proof grade, deleting a proof grade or TCB
    # fail-closed contract, orphaning a formal obligation, reopening an external
    # assumption, or disconnecting a product feature from every end-to-end
    # assurance journey must each make its existing authoritative gate fail. No
    # mutant may introduce a second ledger or a test-only acceptance path.
    mutations = (
        mutate_removed_harness,
        mutate_commented_release_self_test,
        mutate_commented_harness,
        mutate_wrong_harness_package,
        mutate_harness_name_prefix,
        mutate_duplicate_harness_selection,
        mutate_downgraded_claim,
        mutate_unchecked_model,
        mutate_missing_proof_grade,
        mutate_false_production_grade,
        mutate_false_refinement_grade,
        mutate_missing_boundary,
        mutate_invented_boundary,
        mutate_unlinked_product_obligation,
        mutate_reopened_external_assumption,
        mutate_missing_tcb_contract,
        mutate_disconnected_product_feature,
    )
    with tempfile.TemporaryDirectory(prefix="awaken-formal-mutations-") as temp:
        temp_root = Path(temp)
        for index, mutation in enumerate(mutations):
            case_root = temp_root / str(index)
            prepare_case(case_root)
            require_green_baseline(case_root)
            # Only root ledgers and the copied gate are writable mutation
            # targets. Restore exact bytes and prove the baseline again so a
            # secondary fixture failure cannot count as the requested kill.
            targets = [*sorted((case_root / "formal").glob("*.json")), case_root / "scripts/ci/check_formal.sh"]
            original = {path: path.read_bytes() for path in targets}
            mutation(case_root)
            for path, content in original.items():
                path.write_bytes(content)
            require_green_baseline(case_root)
    print(f"formal gate mutation score: {len(mutations)}/{len(mutations)} killed")


def check_memory_copy_model_mutant(tlc_jar: Path) -> None:
    # Cause/effect rule C1: replace ONLY recovery's frozen expected head with
    # the current generation. E1: TLC must report the exact base-preservation
    # invariant, not merely a parse error, timeout or unrelated invariant.
    # The original positive configuration runs immediately before this in CI.
    # All mutation bytes are confined to a fresh temporary tree; no production
    # model is modified and there is no parallel CAS model implementation.
    source = (ROOT / "formal/tla/MemoryCAS.tla").read_text(encoding="utf-8")
    original = """CopyRecover(w) == /\\ crashed[w]
                  /\\ crashed' = [crashed EXCEPT ![w] = FALSE]
                  /\\ UNCHANGED <<vars, mounted, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, pendingAck>>"""
    mutant = """CopyRecover(w) == /\\ crashed[w]
                  /\\ crashed' = [crashed EXCEPT ![w] = FALSE]
                  /\\ expected' = [expected EXCEPT ![w] = generation]
                  /\\ UNCHANGED <<generation, value, writes, sourcePresent, targetPresent,
                                  targetIdentity, renameCommitted, headPresent, lastAction,
                                  lastBefore, lastAfter, lastPresentBefore, lastPresentAfter,
                                  mounted, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, pendingAck>>"""
    if source.count(original) != 1:
        raise SystemExit("Memory copy recovery mutation anchor drifted; review the model")
    # C2: admit delete after an incomplete scan. The exact scan invariant must
    # reject the old missing-root-as-empty behavior; a tool failure cannot pass.
    delete_guard = """CopyDelete(w) == /\\ mounted[w] /\\ ~crashed[w]
                 /\\ scanComplete[w]"""
    if source.count(delete_guard) != 1:
        raise SystemExit("Memory copy scan mutation anchor drifted; review the model")
    root_guard = "                 /\\ rootMatches[w]"
    if source.count(root_guard) != 1:
        raise SystemExit("Memory copy root mutation anchor drifted; review the model")
    mutations = [
        (source.replace(original, mutant), "CopyRecoveryKeepsMaterializationBase"),
        (source.replace(delete_guard, "CopyDelete(w) == /\\ mounted[w] /\\ ~crashed[w]"),
         "CopyMutationRequiresCompleteScan"),
        (source.replace(root_guard, "                 /\\ TRUE"),
         "CopyMutationRequiresOriginalRoot"),
    ]
    config = (ROOT / "formal/tla/MemoryCASCopy.cfg").read_text(encoding="utf-8")
    check_tlc_model_mutations(tlc_jar, "MemoryCAS", source, config,
                              [(mutated, config, invariant) for mutated, invariant in mutations])
    print("Memory copy mutation score: 3/3 killed (frozen base, complete scan, original root)")


def check_agent_input_model_mutant(tlc_jar: Path) -> None:
    # C1 lock omitted before reading absence -> E1 duplicate/stale commit.
    # Require a green original model, then the exact successor invariant failure;
    # parse failures, timeouts and unrelated errors are never mutation kills.
    source = (ROOT / "formal/tla/AgentInputRevision.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/AgentInputRevision.cfg").read_text(encoding="utf-8")
    anchor = "CONSTANT SerializeAdmissions = TRUE"
    if config.count(anchor) != 1:
        raise SystemExit("Agent input serialization mutation anchor drifted; review the model")
    mutant = config.replace(anchor, "CONSTANT SerializeAdmissions = FALSE")
    check_tlc_model_mutations(tlc_jar, "AgentInputRevision", source, config,
                              [(source, mutant, "AppliedIsExactSuccessor")])
    print("Agent input mutation score: 1/1 killed (missing-row admission lock)")


def check_resource_preparation_model_mutant(tlc_jar: Path) -> None:
    # P1 omit early lease visibility (the reproduced bug); P2 let a cancelled
    # preparation publish. Both must violate the named authority invariant,
    # not fail due to syntax, timeout, or infrastructure. Reuse the model/oracle.
    source = (ROOT / "formal/tla/SessionRealizationMutex.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/SessionRealizationMutex.cfg").read_text(encoding="utf-8")
    config = config.replace("MaxCompleted = 3", "MaxCompleted = 1").replace("MaxRenewals = 6", "MaxRenewals = 1")
    visible = "  /\\ leaseVisible' = [leaseVisible EXCEPT ![session] = TRUE]"
    publish = '  /\\ phase[session] = "PreparingResources"\n  /\\ leaseVisible[session]'
    if source.count(visible) != 1 or source.count(publish) != 1:
        raise SystemExit("Resource preparation mutation anchor drifted; review the model")
    mutants = [
        source.replace(visible, visible.replace("TRUE", "FALSE")),
        source.replace(publish, '  /\\ phase[session] \\in {"PreparingResources", "CancellingResources"}'),
    ]
    check_tlc_model_mutations(tlc_jar, "SessionRealizationMutex", source, config,
        [(mutant, config, "ResourcesHaveRenewableAuthority") for mutant in mutants])
    print("Resource preparation mutation score: 2/2 killed (hidden lease, cancelled publication)")


def check_realization_renewal_model_mutant(tlc_jar: Path) -> None:
    # L1: restore expiry-equality catch-up after a completed phase. Successful
    # renewals forever must expose a non-releasing cycle, not an oracle error.
    source = (ROOT / "formal/tla/SessionRealizationMutex.tla").read_text()
    config = (ROOT / "formal/tla/SessionRealizationRenewal.cfg").read_text()
    anchor = "  /\\ phase' = [phase EXCEPT ![session] = \"Committing\"]"
    if source.count(anchor) != 1:
        raise SystemExit("Realization renewal mutation anchor drifted; review the model")
    mutant = source.replace(anchor,
        "  /\\ phase' = [phase EXCEPT ![session] = IF receiptCurrent[session] THEN \"Committing\" ELSE \"Driving\"]")
    check_tlc_model_mutations(tlc_jar, "SessionRealizationMutex", source, config,
        [(mutant, config, "temporal:EveryAcquiredSlotEventuallyReleases")])
    print("Realization renewal mutation score: 1/1 killed (continuous-renewal livelock)")


def check_resolution_failure_model_mutant(tlc_jar: Path) -> None:
    # R1 remove the conflict execution gate -> exact safety violation. R2 the
    # original model must REACH a settled conflict, guarding against a vacuous
    # green configuration (e.g. no live Work lease can ever be acquired).
    source = (ROOT / "formal/tla/SessionRunProtocol.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/SessionRunProtocolResolution.cfg").read_text(encoding="utf-8")
    anchor = "    /\\ run \\notin ConflictedRuns\n"
    if source.count(anchor) != 1:
        raise SystemExit("Resolution conflict mutation anchor drifted; review the model")
    witness = config.replace("INVARIANTS Safety ConflictedRunsNeverExecute", "INVARIANTS Safety NoConflictedRunHasSettled")
    check_tlc_model_mutations(tlc_jar, "SessionRunProtocol", source, config, [
        (source.replace(anchor, ""), config, "ConflictedRunsNeverExecute"),
        (source, witness, "NoConflictedRunHasSettled"),
    ], dependencies=("RunIngressKernel", "SessionActivityKernel", "WorkQueueKernel"))
    print("Resolution conflict: execution mutant killed; terminal settlement reachability witnessed")


def check_config_cas_model_mutant(tlc_jar: Path) -> None:
    # R2: the old unconditional INSERT accepted a nonzero expected revision on
    # absence; R5: removing exhaustion admits a successor outside storage bounds.
    # The shared oracle requires a green baseline and each exact invariant;
    # parser errors and unavailable TLC are never credited as detection.
    source = (ROOT / "formal/tla/AggregateCAS.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/AggregateCAS.cfg").read_text(encoding="utf-8")
    guard = "expected[w] = generation /\\ generation < MaxGeneration"
    if source.count(guard) != 1:
        raise SystemExit("Config CAS mutation anchor drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "AggregateCAS", source, config, [
        (source.replace(guard, "(generation = 0 \\/ expected[w] = generation) /\\ generation < MaxGeneration"),
         config, "AppliedMatchedGeneration"),
        (source.replace(guard, "expected[w] = generation"), config, "TypeOK"),
    ])
    print("Config CAS mutation score: 2/2 killed (absent-row predicate, exhaustion)")


def check_background_completion_model_mutant(tlc_jar: Path) -> None:
    # C1 remove only the uncertainty guard from canonical settlement. E1 the
    # exact unknown-retention invariant fails. Use the same green-base/semantic
    # oracle as other models; an infrastructure failure is never a killed bug.
    source = (ROOT / "formal/tla/BackgroundTask.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/BackgroundTask.cfg").read_text(encoding="utf-8")
    guard = 'p = "Cancelling" /\\ reason # "Indeterminate"'
    if source.count(guard) != 1:
        raise SystemExit("Background completion mutation anchor drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "BackgroundTask", source, config, [
        (source.replace(guard, 'p = "Cancelling"'), config, "CompletionProjectionRetainsUnknown"),
    ])
    print("Background completion mutation score: 1/1 killed (unknown is not cancelled)")


def check_audit_commit_model_mutants(tlc_jar: Path) -> None:
    # A1 remove exact request equality -> wrong intent commits. A3 allow a
    # committed row through the pending guard -> duplicate business write.
    # Each kill requires the green baseline and the named semantic invariant.
    source = (ROOT / "formal/tla/AuditCommit.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/AuditCommit.cfg").read_text(encoding="utf-8")
    exact = r" /\ auditSummary[k] = summary"
    pending = 'auditState[k] = "pending"' + exact
    if source.count(exact) != 1 or source.count(pending) != 1:
        raise SystemExit("Audit commit mutation anchors drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "AuditCommit", source, config, [
        (source.replace(exact, ""), config, "ExactIntentOwnsBusinessWrite"),
        (source.replace(pending, 'auditState[k] # "none"' + exact),
         config, "BusinessCommitIffAuditCommitted"),
    ])
    print("Audit commit mutation score: 2/2 killed (exact intent, committed replay)")


def check_effect_retirement_model_mutant(tlc_jar: Path) -> None:
    # ABA rule: removing exact receipt/current identity matching lets a delayed
    # old worker delete the successor journal without applying its resource state.
    source = (ROOT / "formal/tla/ResourceBindingEffect.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/ResourceBindingEffect.cfg").read_text(encoding="utf-8")
    guard = "IF effectQueued = captured[w] THEN 0 ELSE effectQueued"
    if source.count(guard) != 1:
        raise SystemExit("Effect retirement mutation anchor drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "ResourceBindingEffect", source, config, [
        (source.replace(guard, "0"), config, "CommittedDraftHasAuditAndRecoverableBinding"),
    ])
    print("Effect retirement mutation score: 1/1 killed (stale receipt retires successor)")


def check_credential_removal_model_mutant(tlc_jar: Path) -> None:
    # R1: deleting the absence guard must expose Aborted with residual material,
    # not magically perform the physical deletion inside the WAL transaction.
    source = (ROOT / "formal/tla/ManagedCredentialCreation.tla").read_text()
    config = (ROOT / "formal/tla/ManagedCredentialCreation.cfg").read_text()
    config = config.replace("INVARIANT CreationSafety", "INVARIANTS AbortedCreationIsClean CreationSafety")
    guard = 'CompleteAbortCleanup ==\n    /\\ processUp /\\ phase = "ReclaimingAbort"\n    /\\ material = "None"'
    if source.count(guard) != 1:
        raise SystemExit("Credential removal mutation anchor drifted; review the model")
    mutant = source.replace(guard, guard.rsplit("\n", 1)[0])
    check_tlc_model_mutations(tlc_jar, "ManagedCredentialCreation", source, config,
                             [(mutant, config, "AbortedCreationIsClean")])
    print("Credential removal mutation score: 1/1 killed (unobserved physical deletion)")


def check_session_network_model_mutant(tlc_jar: Path) -> None:
    # N7: the old duplicated intersection admitted the trimmed empty identity.
    # Omitting canonicalization must violate the independent nonblank invariant;
    # parser errors or infrastructure failure are not semantic mutation kills.
    source = (ROOT / "formal/tla/SessionNetworkPolicy.tla").read_text()
    config = (ROOT / "formal/tla/SessionNetworkPolicy.cfg").read_text()
    guard = r"Meet(left, right) == Canonical(left) \intersect Canonical(right)"
    if source.count(guard) != 1:
        raise SystemExit("Session network mutation anchor drifted; review the model")
    mutant = source.replace(guard, r"Meet(left, right) == left \intersect right")
    check_tlc_model_mutations(tlc_jar, "SessionNetworkPolicy", source, config,
                             [(mutant, config, "NoBlankHostIsGranted")])
    print("Session network mutation score: 1/1 killed (blank host is not authority)")


def check_verified_admission_model_mutants(tlc_jar: Path) -> None:
    # R2/R3/R8: omit one target binding, expiry or release guard. Only the named safety
    # violation after a green baseline counts; tool errors never count as kills.
    source = (ROOT / "formal/tla/VerifiedProfileAdmission.tla").read_text()
    config = (ROOT / "formal/tla/VerifiedProfileAdmission.cfg").read_text()
    mutations = []
    for guard, invariant in (
        (r"              /\ \A d \in Dependencies : snapshot.versions[d] = 0", "AcceptedHasExactFingerprint"),
        (r"              /\ \A d \in Dependencies : snapshot.configurations[d] = 0", "AcceptedHasExactFingerprint"),
        (r"                    /\ snapshot.now - snapshot.observed[d] <= 1", "AcceptedHasFreshEvidence"),
        ("ReleaseAdmissible == /\\ release.artifactMatches", "ReleaseConsumptionHasExactExecutedEvidence"),
        (r"                     /\ release.profileMatches", "ReleaseConsumptionHasExactExecutedEvidence"),
        (r"                     /\ release.proofComplete", "ReleaseConsumptionHasExactExecutedEvidence"),
        (r"                     /\ release.sourceMatches", "ReleaseConsumptionHasExactExecutedEvidence"),
    ):
        if source.count(guard) != 1:
            raise SystemExit("Verified admission mutation anchor drifted; review the model")
        replacement = "ReleaseAdmissible == /\\ TRUE" if guard.startswith("ReleaseAdmissible ==") else ""
        mutations.append((source.replace(guard, replacement), config, invariant))
    image_guard = r'IF ~release.imageRequired \/ release.imageEvidence = "ExactSubject"'
    if source.count(image_guard) != 1:
        raise SystemExit("Image payload mutation anchor drifted; review the model")
    mutations.append((source.replace(image_guard, "IF TRUE"), config, "ImageConsumptionHasExactPayload"))
    # I3: a good payload checked under another image identity is not evidence
    # for the promoted subject. Omitting only that binding must expose it.
    mutations.append((source.replace(image_guard, r'IF ~release.imageRequired \/ release.imageEvidence # "Mismatch"'),
                      config, "ImageConsumptionBindsCheckedSubject"))
    check_tlc_model_mutations(tlc_jar, "VerifiedProfileAdmission", source, config, mutations)
    print("Verified admission mutation score: 9/9 killed (target freshness, exact executed release and bound image payload)")


def check_session_observation_model_mutants(tlc_jar: Path) -> None:
    # Decision-table mutation witnesses: M1 unchanged revision resamples time;
    # M2 processed replay resamples; M3 cold recovery invents legacy time;
    # M4 Stage's retired generation escapes into Publish. Every case must have
    # a green baseline and fail its exact invariant/property, not parse/timeout.
    root = (ROOT / "formal/tla/SessionRoot.tla").read_text()
    config = (ROOT / "formal/tla/SessionRoot.cfg").read_text().split("PROPERTIES")[0]
    config += "PROPERTIES NoRevisionChangePreservesTime\n"
    anchor = "    ELSE before\nRootObservationStep"
    assert root.count(anchor) == 1
    mutant = root.replace(anchor, "    ELSE IF before = UnknownTime THEN before ELSE IF now > before THEN now ELSE before\nRootObservationStep")
    check_tlc_model_mutations(tlc_jar, "SessionRoot", root, config,
        [(mutant, config, "action:NoRevisionChangePreservesTime")], dependencies=("SessionRootKernel",))

    event = (ROOT / "formal/tla/SessionEventProtocol.tla").read_text()
    config = (ROOT / "formal/tla/SessionEventProtocol.cfg").read_text().replace("PROPERTY First", "PROPERTIES First")
    anchor = "THEN now ELSE before"
    assert event.count(anchor) == 1
    mutant = event.replace(anchor, 'THEN now ELSE IF wasProcessed THEN now ELSE before')
    check_tlc_model_mutations(tlc_jar, "SessionEventProtocol", event, config,
        [(mutant, config, "action:FirstProcessingTimeIsImmutable")],
        dependencies=("SessionEventKernel", "SessionActivityKernel", "RunIngressKernel"))

    history = (ROOT / "formal/tla/StoreHistory.tla").read_text()
    config = (ROOT / "formal/tla/StoreHistory.cfg").read_text()
    # Small mutation witness only; the unchanged canonical CFG still runs the
    # complete two-identity, three-commit qualification in the strict gate.
    for old, new in [("{t1, t2}", "{t1}"), ("{r1, r2}", "{r1}"), ("{o1, o2}", "{o1}"), ("{p1, p2}", "{p1}")]:
        config = config.replace(old, new)
    anchor = "recoveredPrefix' = history"
    assert history.count(anchor) == 1
    mutant = history.replace(anchor, "recoveredPrefix' = [index \\in 1..Len(history) |-> [history[index] EXCEPT !.observedAt = IF @ = UnknownTime THEN 0 ELSE @]]")
    check_tlc_model_mutations(tlc_jar, "StoreHistory", history, config,
        [(mutant, config, "RecoveryPreservesExactCommittedMetadata")])

    driver = (ROOT / "formal/tla/SessionRealizationMutex.tla").read_text()
    config = (ROOT / "formal/tla/SessionRealizationRenewal.cfg").read_text().replace("INVARIANTS Safety", "INVARIANTS PublicationExcludesRetiredGenerations")
    anchor = 'PublicationCandidates({"old", "new"}, {"new"})'
    assert driver.count(anchor) == 1
    check_tlc_model_mutations(tlc_jar, "SessionRealizationMutex", driver, config,
        [(driver.replace(anchor, '{"old", "new"}'), config, "PublicationExcludesRetiredGenerations")])
    print("Session observation model mutation score: 4/4 killed")


def check_observation_trace_mutants(tlc_jar: Path, trace_dir: Path) -> None:
    from render_runtime_refinement_traces import render_observations, module_name
    manifest = json.loads((ROOT / "formal/runtime-refinement-manifest.json").read_text())
    # X1 bypass binding stamping; X2 resample processed time; X3 publish retired;
    # X4 rewrite recovered metadata. Mutate REAL emitted observations, not the
    # model. Each must violate the exact relation against a green trace baseline.
    for name in ["binding-apply", "processed-first-replay-terminal", "publication-cardinality-renewal", "store-torn-prefix"]:
        document = json.loads((trace_dir / f"{name}.json").read_text())
        with tempfile.TemporaryDirectory(prefix="awaken-observation-mutant-") as temp:
            directory = Path(temp)
            raw, rendered = directory / "raw", directory / "rendered"
            raw.mkdir()
            path = raw / f"{name}.json"
            write_json(path, document)
            render_observations(raw, rendered, ROOT, manifest, False)
            module = module_name(name)
            baseline = (rendered / f"{module}.tla").read_text()
            config = (rendered / f"{module}.cfg").read_text()
            family = document["family"]
            if family == "root":
                step = document["steps"][0]
                step["after"]["updated"] = step["before"]["updated"]
            elif family == "processed":
                document["steps"][0]["after"]["time"] += 1
            elif family == "publication":
                step = next(s for s in document["steps"] if s["drained"])
                step["published"].append(step["drained"][0])
            else:
                document["steps"][0]["recovered"][0]["time"] += 1
            write_json(path, document)
            render_observations(raw, rendered, ROOT, manifest, False)
            mutant = (rendered / f"{module}.tla").read_text()
            assert mutant != baseline
            definition = manifest["session_observations"]["families"][family]
            check_tlc_model_mutations(tlc_jar, module, baseline, config,
                [(mutant, config, "constant:TraceRefines")], dependencies=(definition["model"], *definition["dependencies"]))
    check_observation_rule_mutants(trace_dir, manifest)
    check_realization_trace_mutants(tlc_jar, trace_dir, manifest)
    print("Production observation trace mutation score: 4/4 killed")

def check_cowork_lifecycle_model_mutants(tlc_jar: Path) -> None:
    # C1 restore timeout-as-observer-drop; C2 TERM-as-ACK; C3 crash-as-ACK;
    # C4 first-target-as-complete-fanout. Each mutant must fail its independent
    # invariant after a green baseline, never a syntax/build/infrastructure error.
    source = (ROOT / "formal/tla/AcpBoundary.tla").read_text()
    config = (ROOT / "formal/tla/AcpBoundary.cfg").read_text()
    timeout = source[source.index("CleanupTimeout =="):source.index("ObserveExit(code) ==")]
    mutant_timeout = timeout.replace("killSent, observing, ack", "killSent, ack") + "  /\\ observing' = FALSE\n"
    check_tlc_model_mutations(tlc_jar, "AcpBoundary", source, config, [
        (source.replace(timeout, mutant_timeout), config.replace("INVARIANTS Safety", "INVARIANTS CleanupObserverRetained Safety"), "CleanupObserverRetained"),
        (source.replace("/\\ exitCode # 0 /\\ observing /\\ ~ack", "/\\ termSent /\\ observing /\\ ~ack"), config.replace("INVARIANTS Safety", "INVARIANTS NoFabricatedQuiescence Safety"), "NoFabricatedQuiescence"),
    ])
    source = (ROOT / "formal/tla/SessionThreadToolComposition.tla").read_text()
    config = (ROOT / "formal/tla/SessionThreadToolComposition.cfg").read_text()
    crash = source[source.index("CrashChild(run) =="):source.index("ObserveProcessQuiescence(run) ==")]
    mutant = crash.replace("UNCHANGED <<toolState, physicalRuns, invokedAttempt>>", "UNCHANGED <<toolState, invokedAttempt>>\n    /\\ physicalRuns' = physicalRuns \\ {run}")
    check_tlc_model_mutations(tlc_jar, "SessionThreadToolComposition", source, config, [
        (source.replace(crash, mutant), config.replace("    Safety", "    CrashDoesNotProveQuiescence\n    Safety"), "CrashDoesNotProveQuiescence"),
    ], dependencies=("ThreadCommitProjection",))
    source = (ROOT / "formal/tla/ManagedCredentialRollout.tla").read_text()
    # Safety mutation does not assume eventual provider availability. The full
    # temporal configuration is separately selected by the strict gate.
    config = "SPECIFICATION Spec\nCONSTANT MaxRevision = 3\nINVARIANTS AdoptionCoversEveryNamedFence RolloutSafety\nCHECK_DEADLOCK FALSE\n"
    guard = r"\A target \in Targets: serviceRevision[target] >= deliveryEvent.revision"
    mutant = source.replace(guard, "serviceRevision[1] >= deliveryEvent.revision", 1)
    check_tlc_model_mutations(tlc_jar, "ManagedCredentialRollout", source, config, [
        (mutant, config, "AdoptionCoversEveryNamedFence"),
    ])
    print("Cowork lifecycle mutation score: 4/4 killed (timeout, signal, crash, partial adoption)")


def check_observation_rule_mutants(trace_dir: Path, manifest: dict) -> None:
    from copy import deepcopy
    from render_runtime_refinement_traces import render_observations
    killed = 0
    # Q7 causes: every emitted decision rule independently absent, duplicated,
    # renamed, or replaced with another partition while retaining its label.
    # Effect: coverage rejects before TLC. All originals first render green.
    for name in manifest["session_observations"]["required_scenarios"]:
        original = json.loads((trace_dir / f"{name}.json").read_text())
        with tempfile.TemporaryDirectory() as temp:
            raw, output = Path(temp) / "raw", Path(temp) / "out"
            raw.mkdir()
            path = raw / f"{name}.json"
            write_json(path, original)
            render_observations(raw, output, ROOT, manifest, False)
            for index in range(len(original["steps"])):
                for fault in ["delete", "duplicate", "rename"]:
                    doc = deepcopy(original)
                    if fault == "delete": del doc["steps"][index]
                    elif fault == "duplicate": doc["steps"].append(doc["steps"][index])
                    else: doc["steps"][index]["rule"] = "unknown-rule"
                    write_json(path, doc)
                    try:
                        render_observations(raw, output, ROOT, manifest, False)
                    except ValueError as error:
                        if not any(text in str(error) for text in ["empty observation", "rule coverage drift"]): raise
                        killed += 1
                    else: raise SystemExit(f"coverage mutant survived: {name}/{index}/{fault}")
            if name in {"publication-cardinality-renewal", "binding-clock-cases", "store-torn-prefix", "realization-http-crashes"}:
                for index in range(len(original["steps"])):
                    doc = deepcopy(original)
                    doc["steps"][index] = deepcopy(original["steps"][(index + 1) % len(original["steps"])])
                    doc["steps"][index]["rule"] = original["steps"][index]["rule"]
                    write_json(path, doc)
                    try: render_observations(raw, output, ROOT, manifest, False)
                    except ValueError as error:
                        if "rule conditions drift" not in str(error): raise
                        killed += 1
                    else: raise SystemExit(f"relabeled partition survived: {name}/{index}")
    print(f"Observation rule coverage mutations: {killed}/{killed} killed")


def observation_field_paths(value, schema, path=()):
    """Walk every present wire field, including every array element/phase."""
    yield path, schema, value
    if isinstance(schema, tuple) and schema[0] is None:
        if value is None: return
        schema = schema[1]
    if isinstance(schema, dict):
        for key, child in schema.items(): yield from observation_field_paths(value[key], child, (*path, key))
    elif isinstance(schema, list):
        for index, child in enumerate(value): yield from observation_field_paths(child, schema[0], (*path, index))


def replace_observation_field(value, path, replacement=None, *, delete=False):
    from copy import deepcopy
    result = deepcopy(value)
    parent = result
    for key in path[:-1]: parent = parent[key]
    if delete: del parent[path[-1]]
    else: parent[path[-1]] = replacement
    return result


def check_realization_trace_mutants(tlc_jar: Path, trace_dir: Path, manifest: dict) -> None:
    from copy import deepcopy
    from render_runtime_refinement_traces import (render_observations, module_name, project_realization_trace,
        normalize_observation_times, observation_value, realization_event_schema, validate_observation_schema,
        GENERATION_SCHEMA, LEASE_SCHEMA, OBSERVATION_SET_FIELDS)
    name = "realization-http-crashes"
    original = json.loads((trace_dir / f"{name}.json").read_text())
    # Q8 cause/effect graph: phase -> required wire fields -> shape rejection;
    # complete reference + exact root/plan -> admitted call/report; any changed
    # immutable coordinate or widened expiry -> rejection. Crossing every rule,
    # event occurrence, array element and recovery side prevents one witness
    # (formerly published-2) from substituting for the entire decision table.
    # Q9 null is a fault only for NON-nullable fields. Root legacy optionals
    # retain their wire meaning. Unknown fields/types never become default facts.
    # Q10 deletion/duplication/swapping applies to each reachable phase boundary;
    # swapping two same-phase independent calls is a positive permutation case.
    # Q11 same-generation root renewal is positive: shorter existing evidence
    # remains admissible. Reject-all and equality-only "fixes" must fail here.
    schema_killed, semantic_cases, positives, report = 0, [], [], []
    bases = {}
    def projected(events):
        return normalize_observation_times([{"trace": project_realization_trace(events)}])[0]["trace"]
    def expression(step, events):
        before, after = bases[step["rule"]][1], projected(events)
        changes = []
        if len(before) == len(after):
            for i, (left, right) in enumerate(zip(before, after), 1):
                for key in right:
                    if left[key] != right[key]:
                        changes.append(f'![{i}].{key} = {observation_value(right[key], as_set=key in OBSERVATION_SET_FIELDS)}')
            value = bases[step["rule"]][0]
            if changes: value = "[" + value + " EXCEPT " + ", ".join(changes) + "]"
        else: value = observation_value(after)
        return f'RealizationTraceIsRefinement({value}, {observation_value(step["cut"])}, {step["count"]})'
    for index, step in enumerate(original["steps"]):
        events = step["trace"]
        bases[step["rule"]] = (f"Base{index}", projected(events))
        positives.append(expression(step, events))
        for event_index, event in enumerate(events):
            schema = realization_event_schema(event)
            for path, field_schema, value in observation_field_paths(event, schema):
                if not path: continue
                nullable = isinstance(field_schema, tuple) and field_schema[0] is None
                underlying = field_schema[1] if nullable else field_schema
                invalid = [] if isinstance(underlying, dict) else {} if isinstance(underlying, list) else True if underlying == "uint" else 0 if underlying == "bool" else False
                faults = [("type", invalid)] + ([] if nullable else [("null", None)])
                if underlying == "text": faults.append(("empty", ""))
                elif underlying == "uint": faults.extend([("negative", -1), ("overflow", 2**64), ("fraction", 1.5)])
                elif isinstance(underlying, tuple): faults.append(("unknown-enum", "unknown-observation"))
                elif isinstance(underlying, list) and value: faults.append(("duplicate-element", value + [value[0]]))
                if isinstance(path[-1], str): faults.append(("missing", None))
                for fault, replacement in faults:
                    mutant = replace_observation_field(event, path, replacement, delete=fault == "missing")
                    try: validate_observation_schema(mutant, realization_event_schema(mutant), "event")
                    except ValueError as error:
                        if "observation schema violation" not in str(error): raise
                        schema_killed += 1
                    else: raise SystemExit(f"schema mutant survived: {step['rule']}/{event_index}/{path}/{fault}")
                if isinstance(underlying, dict):
                    mutant = replace_observation_field(event, path, dict(value or {}, unexpected_observation=True))
                    try: validate_observation_schema(mutant, realization_event_schema(mutant), "event")
                    except ValueError as error:
                        if "observation schema violation" not in str(error): raise
                        schema_killed += 1
                    else: raise SystemExit(f"unknown-field mutant survived: {step['rule']}/{event_index}/{path}")
                if field_schema == [GENERATION_SCHEMA]:
                    # Q13 collection completeness is checked even when a crash
                    # prevents the omitted second effect from executing.
                    for item in range(len(value)):
                        mutated = deepcopy(events)
                        mutated[event_index] = replace_observation_field(event, (*path, item), delete=True)
                        semantic_cases.append((step, mutated, f"{step['rule']}/{event_index}/{path}/missing-element-{item}"))
                reference = field_schema == GENERATION_SCHEMA
                asserted_lease = field_schema == LEASE_SCHEMA and event["point"] in {"projection", "stage", "published", "complete"}
                if reference or asserted_lease:
                    for field, old in value.items():
                        new = old + (999999999 if "expires" in field else 999) if type(old) is int else "foreign-observation"
                        mutated = deepcopy(events)
                        mutated[event_index] = replace_observation_field(event, (*path, field), new)
                        semantic_cases.append((step, mutated, f"{step['rule']}/{event_index}/{path}/{field}"))
            for field in ("sequence", "session"):
                mutated = deepcopy(events)
                mutated[event_index][field] = event[field] + 999 if field == "sequence" else "foreign-session"
                semantic_cases.append((step, mutated, f"{step['rule']}/{event_index}/{field}"))
            for operation in ("delete", "duplicate", "swap"):
                if operation == "swap" and event_index + 1 == len(events): continue
                mutated = deepcopy(events)
                if operation == "delete": del mutated[event_index]
                elif operation == "duplicate": mutated.insert(event_index, deepcopy(event))
                else: mutated[event_index], mutated[event_index + 1] = mutated[event_index + 1], mutated[event_index]
                for i, item in enumerate(mutated): item["sequence"] = i
                label = f"{step['rule']}/{event_index}/{operation}"
                if operation == "swap" and event["point"] == events[event_index + 1]["point"]:
                    positives.append(expression(step, mutated))
                else: semantic_cases.append((step, mutated, label))
        renewed = deepcopy(events)
        for event in renewed:
            data = event["data"]
            if "attachments" in data:
                if data["lease"]: data["lease"]["expires_at_unix_ms"] += 1000
                for attachment in data["attachments"]:
                    if attachment["claim"]: attachment["claim"]["lease_expires_at_unix_ms"] += 1000
        positives.append(expression(step, renewed))

    artifact_root = os.environ.get("AWAKEN_FORMAL_MUTATION_EVIDENCE_DIR")
    with tempfile.TemporaryDirectory(prefix="awaken-observation-matrix-") as temp:
        raw, rendered = Path(temp) / "raw", Path(temp) / "rendered"
        raw.mkdir()
        write_json(raw / f"{name}.json", original)
        render_observations(raw, rendered, ROOT, manifest, False)
        module = module_name(name)
        config = (rendered / f"{module}.cfg").read_text()
        declarations = "\n".join(f"{symbol} == {observation_value(trace)}" for symbol, trace in bases.values())
        prelude = f"---- MODULE {module} ----\nEXTENDS SessionRealizationMutex\n" + declarations + "\nTraceInit == Init\nTraceNext == UNCHANGED vars\nTraceSpec == TraceInit /\\ [][TraceNext]_vars\n"
        def model(expressions): return prelude + "TraceRefines == /\\ " + "\n    /\\ ".join(expressions) + "\n" + "=" * 77 + "\n"
        check_tlc_model_mutations(tlc_jar, module, model(positives), config, [], dependencies=("SessionRealizationMutex",), artifact_directory=Path(artifact_root) / "positive-controls" if artifact_root else None)
        negatives = []
        # Q12 independently exercise the model's required-authority guard even
        # though v3 schema rejects missing raw leases before model projection.
        for step in original["steps"]:
            symbol, states = bases[step["rule"]]
            for index, state in enumerate(states, 1):
                if state["event"] not in {"begin-before", "crash", "publish-one", "drain-one"}:
                    negatives.append(f'~RealizationTraceIsRefinement([{symbol} EXCEPT ![{index}].hasauthority = FALSE], {observation_value(step["cut"])}, {step["count"]})')
                    report.append({"case": f"{step['rule']}/{index - 1}/required-authority", "oracle": "TLC-projection-refutation"})
        for step, events, label in semantic_cases:
            try: expr = expression(step, events)
            except ValueError as error:
                if "must begin with an observed root" not in str(error): raise
                report.append({"case": label, "oracle": "missing-initial-root"})
                continue
            if expr == expression(step, step["trace"]): raise SystemExit(f"erased semantic mutant: {label}")
            negatives.append("~(" + expr + ")")
            report.append({"case": label, "oracle": "TLC-refutation"})
        # Each conjunction asserts rejection of EVERY counterfactual, never
        # "at least one fails". The shared oracle requires clean TLC completion;
        # parser/tool/timeout errors and any surviving case fail the gate.
        if artifact_root:
            write_json(Path(artifact_root) / "cases.json", {"schema_rejections": schema_killed, "positive_controls": len(positives), "cases": report})
        for offset in range(0, len(negatives), 64):
            source = model(negatives[offset:offset + 64])
            check_tlc_model_mutations(tlc_jar, module, source, config, [], dependencies=("SessionRealizationMutex",),
                artifact_directory=Path(artifact_root) / f"batch-{offset // 64}" if artifact_root else None)
    print(f"Observation schema mutations: {schema_killed}/{schema_killed} rejected")
    print(f"Continuous observation matrix: {len(report)}/{len(report)} rejected; {len(positives)} positive controls passed; {len(bases)} rules")


def check_tlc_model_mutations(tlc_jar: Path, name: str, source: str, config: str,
                              mutations: list[tuple[str, str, str]], *,
                              dependencies: tuple[str, ...] = (), artifact_directory: Path | None = None) -> None:
    # One TLC invocation/oracle for every model mutation family. Validate both
    # the process result and semantic outcome; always establish the green base.
    cases = [(source, config, None), *mutations]
    for model_text, config_text, invariant in cases:
        with tempfile.TemporaryDirectory(prefix="awaken-tlc-mutant-") as temp:
            directory = Path(temp)
            for dependency in dependencies:
                shutil.copy2(ROOT / f"formal/tla/{dependency}.tla", directory)
            model = directory / f"{name}.tla"
            model.write_text(model_text, encoding="utf-8")
            config_path = directory / f"{name}.cfg"
            config_path.write_text(config_text, encoding="utf-8")
            if artifact_directory:
                artifact_directory.mkdir(parents=True, exist_ok=False)
                for source_path in directory.glob("*"):
                    if source_path.is_file(): shutil.copy2(source_path, artifact_directory / source_path.name)
            try:
                result = subprocess.run(
                    ["java", "-Xmx512m", "-jar", str(tlc_jar.resolve()),
                     "-metadir", str(directory / "states"), "-config", str(config_path), str(model)],
                    cwd=directory, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                    timeout=30, check=False,
                )
            except (subprocess.TimeoutExpired, OSError) as error:
                if artifact_directory: (artifact_directory / "tlc.log").write_text(f"proof tool failed: {error}")
                raise
            if artifact_directory:
                (artifact_directory / "tlc.log").write_text(result.stdout)
            temporal = invariant is not None and invariant.startswith("temporal:")
            action = invariant is not None and invariant.startswith("action:")
            constant = invariant is not None and invariant.startswith("constant:")
            if temporal:
                # TLC does not name a failed temporal property in its output.
                # Restrict this oracle to one explicitly named CFG property.
                properties = [line.strip() for line in config_text.splitlines()
                              if line.strip().startswith(("PROPERTY ", "PROPERTIES "))]
                if properties != [f"PROPERTIES {invariant.removeprefix('temporal:')}"]:
                    raise SystemExit("temporal mutation requires one exact named property")
            expected_code = 0 if invariant is None else (151 if constant else 13 if temporal or action else 12)
            expected_text = ("Model checking completed. No error has been found." if invariant is None
                             else f"The invariant of {invariant.removeprefix('constant:')} is equal to FALSE" if constant
                             else f"Action property {invariant.removeprefix('action:')} is violated." if action
                             else "Temporal properties were violated." if temporal
                             else f"Invariant {invariant} is violated")
            if result.returncode != expected_code or expected_text not in result.stdout:
                raise SystemExit(f"{name} {'baseline failed' if invariant is None else 'mutant was not killed correctly'} (exit={result.returncode}):\n{result.stdout}")


if __name__ == "__main__":
    main()
