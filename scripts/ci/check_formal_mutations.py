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

from formal_model_mutation_runner import check_tlc_model_mutations
from formal_memory_mutations import (
    check_memory_extraction_model_mutants,
    check_memory_invalidation_model_mutants,
    check_memory_participant_model_mutants,
)
from formal_execution_replacement_mutations import (
    check_execution_replacement_model_mutants, check_k8s_process_observation_model_mutants,
)
from formal_credential_mutations import (
    check_mcp_credential_model_mutants, check_credential_effect_model_mutants,
)
from formal_dispatch_mutations import check_resolution_failure_model_mutant, check_terminal_scan_model_mutants


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
    def test_model_cli_flags_keep_their_exact_single_handler(self) -> None:
        # R1 each existing flag => exactly its handler, once, with parsed Path.
        # This compatibility list is independent of the production registry;
        # a lost registration or mismatched dispatch must fail before real TLC.
        flags = (
            "memory-copy-model-mutant", "agent-input-model-mutant",
            "resource-preparation-model-mutant", "realization-renewal-model-mutant",
            "resolution-failure-model-mutant", "terminal-scan-model-mutants", "config-cas-model-mutant",
            "background-completion-model-mutant", "sandbox-control-model-mutants",
            "audit-commit-model-mutants", "effect-retirement-model-mutant",
            "credential-removal-model-mutant", "verified-admission-model-mutants",
            "session-network-model-mutant", "session-observation-model-mutants",
            "cowork-lifecycle-model-mutants", "local-process-model-mutant",
            "local-recovery-model-mutants", "checkpoint-recovery-model-mutants",
            "mcp-credential-model-mutants", "credential-effect-model-mutants",
        )
        for flag in flags:
            handler = "check_" + flag.replace("-", "_")
            if flag == "local-process-model-mutant": handler += "s"
            with self.subTest(flag=flag), patch(f"{__name__}.{handler}") as check:
                with patch.object(sys, "argv", ["checker", f"--{flag}", "tlc.jar"]): main()
                check.assert_called_once_with(Path("tlc.jar"))

    def test_tlc_launch_keeps_the_resource_bound_and_strict_timeout(self) -> None:
        # R1 default and R2 explicit composed-model budgets reach every child
        # invocation. Both still require a green baseline and the exact named
        # invariant counterexample; no timeout or merely nonzero exit is a kill.
        # R3 budgets/worker count keep the canonical collector and full configuration;
        # R4 a timeout escapes as failed verification, never a semantic kill.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        rejected = subprocess.CompletedProcess([], 12, "Invariant ExactRule is violated")
        for budget, heap, timeout, workers in (({}, "-Xmx512m", 60, "1"),
                                     ({"heap_mb": 2048, "timeout_seconds": 600, "workers": 4}, "-Xmx2048m", 600, "4")):
            with self.subTest(budget=budget), patch("subprocess.run", side_effect=[green, rejected]) as runner:
                check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config",
                                          [("mutant", "config", "ExactRule")], **budget)
                self.assertEqual(runner.call_count, 2)
                for call in runner.call_args_list:
                    self.assertEqual(call.args[0][:5], ["java", f'-Djava.io.tmpdir={call.kwargs["cwd"]}', heap, "-XX:+UseParallelGC", "-jar"])
                    self.assertEqual(call.kwargs["timeout"], timeout)
                    argv = call.args[0]
                    self.assertEqual(argv[argv.index("-workers") + 1], workers)
            with patch("subprocess.run", side_effect=subprocess.TimeoutExpired("java", timeout)):
                with self.assertRaises(subprocess.TimeoutExpired):
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config", [], **budget)

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
        # Isolation rule: each invocation extracts standard modules inside its
        # own temporary owner; a peer exit cannot delete this JVM's parser input.
        green = subprocess.CompletedProcess([], 0, "Model checking completed. No error has been found.")
        for code, output, accepted in (
            (12, "Invariant ExactRule is violated", True),
            (0, "Invariant ExactRule is violated", False),
            (12, "Invariant OtherRule is violated", False),
            (1, "parse error", False),
            (150, "NullPointerException: parsing failed", False),
            (-9, "Invariant ExactRule is violated", False),
        ):
            with self.subTest(code=code, output=output), patch("subprocess.run", side_effect=[
                green, subprocess.CompletedProcess([], code, output),
            ]) as runner:
                if accepted:
                    check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config",
                                              [("mutant", "config", "ExactRule")])
                else:
                    with self.assertRaisesRegex(SystemExit, "not killed correctly"):
                        check_tlc_model_mutations(Path("tlc.jar"), "Fixture", "source", "config",
                                                  [("mutant", "config", "ExactRule")])
                calls = runner.call_args_list
                self.assertEqual(len({call.kwargs["cwd"] for call in calls}), 2)
                for call in calls: self.assertIn(f'-Djava.io.tmpdir={call.kwargs["cwd"]}', call.args[0])
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
    # One registry owns both accepted CLI flags and their execution handlers.
    model_checks = {
        "memory-copy-model-mutant": check_memory_copy_model_mutant,
        "agent-input-model-mutant": check_agent_input_model_mutant,
        "resource-preparation-model-mutant": check_resource_preparation_model_mutant,
        "realization-renewal-model-mutant": check_realization_renewal_model_mutant,
        "resolution-failure-model-mutant": check_resolution_failure_model_mutant,
        "terminal-scan-model-mutants": check_terminal_scan_model_mutants,
        "k8s-process-observation-model-mutants": check_k8s_process_observation_model_mutants,
        "config-cas-model-mutant": check_config_cas_model_mutant,
        "background-completion-model-mutant": check_background_completion_model_mutant,
        "sandbox-control-model-mutants": check_sandbox_control_model_mutants,
        "memory-invalidation-model-mutants": check_memory_invalidation_model_mutants,
        "memory-participant-model-mutants": check_memory_participant_model_mutants,
        "memory-extraction-model-mutants": check_memory_extraction_model_mutants,
        "audit-commit-model-mutants": check_audit_commit_model_mutants,
        "effect-retirement-model-mutant": check_effect_retirement_model_mutant,
        "credential-removal-model-mutant": check_credential_removal_model_mutant,
        "verified-admission-model-mutants": check_verified_admission_model_mutants,
        "session-network-model-mutant": check_session_network_model_mutant,
        "session-observation-model-mutants": check_session_observation_model_mutants,
        "cowork-lifecycle-model-mutants": check_cowork_lifecycle_model_mutants,
        "local-process-model-mutant": check_local_process_model_mutants,
        "local-recovery-model-mutants": check_local_recovery_model_mutants,
        "checkpoint-recovery-model-mutants": check_checkpoint_recovery_model_mutants,
        "mcp-credential-model-mutants": check_mcp_credential_model_mutants,
        "credential-effect-model-mutants": check_credential_effect_model_mutants,
    }
    model_selection = parser.add_mutually_exclusive_group()
    for name in model_checks:
        model_selection.add_argument(f"--{name}", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--session-observation-trace-mutants", type=Path, metavar="TLC_JAR")
    parser.add_argument("--observation-trace-dir", type=Path)
    arguments = parser.parse_args()
    if arguments.session_observation_trace_mutants is not None:
        if arguments.observation_trace_dir is None:
            parser.error("--observation-trace-dir is required")
        check_observation_trace_mutants(arguments.session_observation_trace_mutants, arguments.observation_trace_dir)
        return
    for name, check in model_checks.items():
        if (jar := getattr(arguments, name.replace("-", "_"))) is not None:
            check(jar)
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


def check_local_process_model_mutants(tlc_jar: Path) -> None:
    # Cause/effect rules LP1 missing atomic spawn policy -> unconfigured handle;
    # LP2 backoff retains mutex -> peer-poll exclusion; LP3 cancelled waiter
    # retains mutex -> abandoned owner; LP4 crash erases descendants -> a false
    # OS guarantee has been smuggled into the model; LP5 a denied group signal
    # succeeds after leader exit -> false cleanup acknowledgement. Each violates
    # its named property; the shared TLC runner rejects parse/tool failures as kills.
    source = (ROOT / "formal/tla/LocalProcessLifecycle.tla").read_text(encoding="utf-8")
    config = (ROOT / "formal/tla/LocalProcessLifecycle.cfg").read_text(encoding="utf-8")
    replacements = [
        ("privateGroup' = TRUE /\\ dropPolicy' = TRUE",
         "privateGroup' = FALSE /\\ dropPolicy' = TRUE", "ConfiguredBeforeSpawn"),
        ("/\\ lock' = NoLock\n  /\\ waiter' = [waiter EXCEPT ![w] = IF leaderAlive",
         "/\\ lock' = IF leaderAlive THEN w ELSE NoLock\n  /\\ waiter' = [waiter EXCEPT ![w] = IF leaderAlive",
         "NoParkedWaiterOwnsLock"),
        ("lock' = IF lock = w THEN NoLock ELSE lock", "lock' = lock", "NoParkedWaiterOwnsLock"),
        ("/\\ UNCHANGED <<privateGroup, dropPolicy, leaderAlive, descendantAlive,\n                  stopRequested, observedExit, signalVars>>",
         "/\\ descendantAlive' = FALSE\n  /\\ UNCHANGED <<privateGroup, dropPolicy, leaderAlive,\n                  stopRequested, observedExit, signalVars>>",
         "action:CrashPreservesOSProcesses"),
        ('signalAccepted\' = (outcome \\in {"accepted", "absent"})',
         'signalAccepted\' = (outcome \\in {"accepted", "absent"} \\/ ~leaderAlive)',
         "SignalDenialIsNotAcknowledged"),
    ]
    mutations = []
    for original, replacement, property_name in replacements:
        if source.count(original) != 1:
            raise SystemExit(f"Local process mutation anchor drifted: {property_name}")
        mutations.append((source.replace(original, replacement), config, property_name))
    check_tlc_model_mutations(tlc_jar, "LocalProcessLifecycle", source, config, mutations)
    print("Local process mutation score: 5/5 killed (spawn, backoff, cancellation, crash, denied signal)")
    # CG: close cannot pass an admitted launch; a leader-only observation or
    # empty observation without an accepted stop cannot publish quiescence.
    scope_config = (ROOT / "formal/tla/LocalProcessScope.cfg").read_text(encoding="utf-8")
    scope_replacements = [
        # B27 revoke after admission/before GO: removing the final authority
        # check must violate the grant receipt, not an unrelated type property.
        ("/\\ ~authorityHeld /\\ authorityLive\n  /\\ authorityHeld' = TRUE",
         "/\\ ~authorityHeld\n  /\\ authorityHeld' = TRUE", "ScopeGrantWasAuthorized"),
        # G5 a separate thread revokes between check and GO if it does not
        # contend on the SAME guard. No-await alone cannot kill this mutant.
        ("/\\ ~authorityHeld /\\ authorityLive /\\ authorityLive' = FALSE",
         "/\\ authorityLive /\\ authorityLive' = FALSE", "ScopeGrantWasAuthorized"),
        ("/\\ ~scopePermit /\\ scopeClosed' = TRUE", "/\\ scopeClosed' = TRUE",
         "ScopeCloseExcludesLaunch"),
        ("/\\ scopeClosed /\\ scopeKillAccepted /\\ ~leaderAlive /\\ ~descendantAlive",
         "/\\ scopeClosed /\\ scopeKillAccepted /\\ ~leaderAlive", "ScopeReceiptRequiresEmpty"),
        ("/\\ scopeClosed /\\ scopeKillAccepted /\\ ~leaderAlive /\\ ~descendantAlive",
         "/\\ scopeClosed /\\ ~leaderAlive /\\ ~descendantAlive", "ScopeReceiptRequiresAcceptedStop"),
        # B14 execute before join/GO; B15 release admission before ACK; B16
        # cancellation fabricates GO. Each must fail the named handshake rule,
        # not a parser/tool error or an unrelated invariant.
        ("/\\ leaderAlive /\\ goIssued /\\ joined /\\ ~workloadStarted",
         "/\\ leaderAlive /\\ ~workloadStarted", "ScopeWorkloadRequiresGrant"),
        ('/\\ phase = "new" /\\ scopePermit /\\ scopePermit\' = FALSE',
         "/\\ scopePermit /\\ scopePermit' = FALSE", "ScopePendingJoinExcludesClose"),
        ('/\\ launchStage\' = "cancelled"\n  /\\ UNCHANGED <<vars, scopeClosed, scopeKillAccepted, scopeReceipt,\n                  joined, goIssued, workloadStarted>>',
         '/\\ launchStage\' = "cancelled" /\\ goIssued\' = TRUE\n  /\\ UNCHANGED <<vars, scopeClosed, scopeKillAccepted, scopeReceipt,\n                  joined, workloadStarted>>',
         "ScopeCancelNeverGrants"),
    ]
    scope_mutations = []
    for original, replacement, property_name in scope_replacements:
        if source.count(original) != 1:
            raise SystemExit(f"Local scope mutation anchor drifted: {property_name}")
        scope_mutations.append((source.replace(original, replacement), scope_config, property_name))
    check_tlc_model_mutations(tlc_jar, "LocalProcessLifecycle", source, scope_config, scope_mutations)
    print("Local scope mutation score: 8/8 killed (final authority, revoke/grant exclusion, launch/close, descendants, accepted stop, early exec, early release, cancelled GO)")
    # NS decision table: NS1 workload exits before namespace init => no receipt;
    # NS2 init teardown leaves a detached member => not quiescent; NS3 exec after
    # helper/control/deadline loss => reject. NS4 each failure link must eventually
    # begin teardown, given kernel progress. Mutants remove one cause at a time.
    # Safety/action failures require the exact name. For liveness TLC supplies
    # no name, so each mutant CFG selects ONLY the one named temporal property.
    boundary_config = (ROOT / "formal/tla/LocalProcessBoundary.cfg").read_text(encoding="utf-8")
    boundary_replacements = [
        ('((~scopeReceipt /\\ scopeReceipt\') => initState \\in {"absent", "exited"})',
         "TRUE", "BoundaryReceiptObservesInit"),
        ("/\\ leaderAlive' = FALSE /\\ descendantAlive' = FALSE\n  /\\ helperAlive' = FALSE",
         "/\\ leaderAlive' = FALSE /\\ UNCHANGED descendantAlive\n  /\\ helperAlive' = FALSE",
         "BoundaryExitedHasNoMembers"),
        ("((~workloadStarted /\\ workloadStarted') => BoundaryRunnable)",
         "TRUE", "action:BoundaryExecRequiresLiveCutoff"),
    ]
    boundary_mutations = []
    for original, replacement, property_name in boundary_replacements:
        if source.count(original) != 1:
            raise SystemExit(f"Local boundary mutation anchor drifted: {property_name}")
        boundary_mutations.append((source.replace(original, replacement), boundary_config, property_name))
    temporal_config = boundary_config.split("PROPERTIES\n", 1)[0]
    temporal_config += "PROPERTIES BoundaryFailureEventuallyTerminates\n"
    for cause in ["phase = \"crashed\"", "~helperAlive", "~controlLive", "~deadlineLive"]:
        original = ('initState = "live" /\\ (phase = "crashed" \\/ ~helperAlive \\/ ~controlLive\n'
                    '    \\/ ~deadlineLive \\/ scopeKillAccepted \\/ launchStage = "cancelled"\n'
                    '    \\/ (workloadStarted /\\ ~leaderAlive))')
        # Preserve the property antecedent; weaken only the transition guard,
        # otherwise the obligation would disappear along with the implementation.
        replacement = original.replace(cause, "FALSE")
        guard = '/\\ BoundaryStopRequired /\\ initState\' = "stopping"'
        if source.count(guard) != 1:
            raise SystemExit("Local boundary stop mutation anchor drifted")
        mutant = source.replace(guard, f'/\\ ({replacement}) /\\ initState\' = "stopping"')
        boundary_mutations.append((mutant, temporal_config, "temporal:BoundaryFailureEventuallyTerminates"))
    check_tlc_model_mutations(tlc_jar, "LocalProcessLifecycle", source, boundary_config,
                              boundary_mutations)
    print("Local boundary mutation score: 7/7 killed (init receipt, detached member, expired exec, owner/helper/control/deadline links)")
    # R1 no held current authority at write => unauthorized frame; R2 stale
    # frame overwrites cutoff => shortening; R3 late frame bypasses expiry =>
    # resurrection; R4 receive-time arithmetic => minted transit time; R5 a
    # stalled prefix suppresses clock progress => lost cutoff. Isolate each
    # named property so unrelated type/invariant failures cannot count as kills.
    renewal_config = (ROOT / "formal/tla/LocalProcessRenewal.cfg").read_text(encoding="utf-8")
    renewal_header = renewal_config.split("INVARIANTS\n", 1)[0]
    renewal_replacements = [
        ('/\\ workloadStarted /\\ authorityHeld /\\ controlLive',
         '/\\ workloadStarted /\\ controlLive',
         "RenewalWritesRequireCurrentAuthority", False),
        ("IF Head(cutoffQueue) > cutoff THEN Head(cutoffQueue) ELSE cutoff",
         "Head(cutoffQueue)", "RenewalCutoffNeverDecreases", True),
        ('/\\ initState = "live" /\\ helperAlive /\\ controlLive /\\ deadlineLive\n  /\\ cutoffQueue',
         '/\\ initState = "live" /\\ helperAlive /\\ controlLive\n  /\\ cutoffQueue',
         "RenewalCannotResurrect", True),
        ("IF Head(cutoffQueue) > cutoff THEN Head(cutoffQueue) ELSE cutoff",
         "IF Head(cutoffQueue) > cutoff THEN Head(cutoffQueue) + cutoffClock ELSE cutoff",
         "RenewalCannotMintTransitTime", True),
        ("/\\ cutoffClock < 4 /\\ cutoffClock' = cutoffClock + 1",
         "/\\ ~cutoffPartial /\\ cutoffClock < 4 /\\ cutoffClock' = cutoffClock + 1",
         "temporal:RenewalPartialCannotStallClock", False),
    ]
    renewal_mutations = []
    for original, replacement, property_name, action in renewal_replacements:
        if source.count(original) != 1:
            raise SystemExit(f"Local renewal mutation anchor drifted: {property_name}")
        keyword = "PROPERTIES" if action or property_name.startswith("temporal:") else "INVARIANTS"
        selected = property_name.removeprefix("temporal:")
        selected_config = renewal_header + f"{keyword} {selected}\n"
        renewal_mutations.append((source.replace(original, replacement), selected_config,
                                  "action:" + property_name if action else property_name))
    check_tlc_model_mutations(tlc_jar, "LocalProcessLifecycle", source, renewal_config,
                              renewal_mutations)
    print("Local renewal mutation score: 5/5 killed (current grant, stale cutoff, resurrection, transit time, partial stall)")
    check_local_recovery_model_mutants(tlc_jar)
    # Refresh classification reuses the exact kernel model. F1 swallowing all
    # errors must fail safety; F2 the old unconditional error must fail benign
    # exit handling; F3 leader-only observation must fail exact-tree safety.
    # Positive witnesses additionally exclude an inert/unreachable classifier.
    refresh_source = (ROOT / "formal/tla/LocalProcessRefresh.tla").read_text(encoding="utf-8")
    refresh_config = (ROOT / "formal/tla/LocalProcessRefresh.cfg").read_text(encoding="utf-8")
    refresh_header = refresh_config.split("INVARIANTS\n", 1)[0]
    refresh_mutations = []
    for original, replacement, property_name in [
        ('IF observation = "exited" THEN "ignored" ELSE "error"',
         '"ignored"', "IgnoredRefreshRequiresExactExit"),
        ('IF observation = "exited" THEN "ignored" ELSE "error"',
         '"error"', "ConfirmedExitSkipsRefreshFailure"),
        ('ELSE IF initState = "exited" THEN "exited" ELSE "live"',
         'ELSE IF ~leaderAlive THEN "exited" ELSE "live"', "IgnoredRefreshRequiresExactExit"),
    ]:
        if refresh_source.count(original) != 1:
            raise SystemExit(f"Local refresh mutation anchor drifted: {property_name}")
        refresh_mutations.append((refresh_source.replace(original, replacement),
                                  refresh_header + f"INVARIANTS {property_name}\n", property_name))
    for witness in ("NoRefreshSkipReached", "NoRefreshErrorReached"):
        refresh_mutations.append((refresh_source, refresh_header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "LocalProcessRefresh", refresh_source, refresh_config,
                              refresh_mutations, dependencies=("LocalProcessLifecycle",))
    print("Local refresh mutation score: 3/3 killed; both skip/error outcomes reachable")
    # Repository R1-R3 compose the same kernel. Test response-only acceptance,
    # leader-only wait, malformed response, failed exit, cancelled caller and
    # Worker crash separately; witnesses require useful and adverse schedules.
    source = (ROOT / "formal/tla/LocalRepositoryEffect.tla").read_text()
    config = (ROOT / "formal/tla/LocalRepositoryEffect.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for replacements, oracle, temporal in [
        ([('/\\ repositoryReply = "valid" /\\ repositoryStatus = "success"\n  /\\ initState = "exited"',
           '/\\ repositoryReply = "valid"')], "RepositoryReturnRequiresExit", False),
        ([('workloadStarted /\\ initState = "exited" /\\ repositoryStatus = "none"',
           'workloadStarted /\\ ~leaderAlive /\\ repositoryStatus = "none"'),
          ('\n  /\\ initState = "exited"\n  /\\ repositoryCaller\'',
           '\n  /\\ ~leaderAlive\n  /\\ repositoryCaller\'')],
         "RepositoryReturnRequiresExit", False),
        ([('repositoryReply = "valid" /\\ repositoryStatus = "success"',
           'repositoryReply # "none" /\\ repositoryStatus = "success"')],
         "RepositoryReturnRequiresValidReply", False),
        ([('repositoryReply = "valid" /\\ repositoryStatus = "success"',
           'repositoryReply = "valid" /\\ repositoryStatus # "none"')],
         "RepositoryReturnRequiresSuccess", False),
        ([('repositoryCaller = "waiting" /\\ phase = "owned"',
           'repositoryCaller # "returned" /\\ phase = "owned"')],
         "RepositoryCancellationIsTerminal", True),
        ([('repositoryCaller = "waiting" /\\ phase = "owned"',
           'repositoryCaller = "waiting"')],
         "RepositoryCrashCannotReturn", True),
    ]:
        mutant = source
        for anchor, broken in replacements:
            if mutant.count(anchor) != 1:
                raise SystemExit(f"Repository effect mutation anchor drifted: {oracle}")
            mutant = mutant.replace(anchor, broken)
        keyword = "PROPERTIES" if temporal else "INVARIANTS"
        mutations.append((mutant, header + f"{keyword} {oracle}\n", "action:" + oracle if temporal else oracle))
    for witness in ("NoRepositoryReturnReached", "NoEarlyRepositoryReplyReached",
                    "NoCancelledRepositoryExitReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "LocalRepositoryEffect", source, config, mutations,
                              dependencies=("LocalProcessLifecycle",))
    print("Local Repository effect mutation score: 6/6 killed; 3 schedules reachable")
    # D9-D14: sampling the OS anchor AFTER the authority wait must mint time;
    # removing expiry rejection must invoke a stale effect. Each oracle is exact.
    source = (ROOT / "formal/tla/LocalProcessDeadline.tla").read_text()
    config = (ROOT / "formal/tla/LocalProcessDeadline.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for anchor, broken, invariant in [
        ("candidate == budgetAnchor + budgetRemaining",
         "candidate == cutoffClock + budgetRemaining", "ConversionNeverMintsAuthorityWaitTime"),
        ('IF candidate > cutoffClock THEN "accepted" ELSE "rejected"',
         '"accepted"', "ExpiredConversionHasNoEffect"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Local deadline mutation anchor drifted")
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    for witness in ("NoAcceptedConversionReached", "NoExpiredConversionReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    check_tlc_model_mutations(tlc_jar, "LocalProcessDeadline", source, config, mutations,
                              dependencies=("LocalProcessLifecycle",))
    print("Local deadline mutation score: 2/2 killed; accepted/expired conversion reachable")


def check_local_recovery_model_mutants(tlc_jar: Path) -> None:
    # One authority for the former recovery mutants and the async participant
    # extension. REC1-6 retain GO/OS/Hand checks. D1 cancellation loses the file
    # guard; D2 steals another borrow; D3-5/D9 skip fresh checks; D6-7 admit
    # false control/unmount completion; D8 records Removed with a live root;
    # D10 skips post-wait quiescence; D11 skips only the NEXT mount admission;
    # D12 forgets the complete guard set after an expired final teardown.
    # Exact oracles and a green base are required.
    source = (ROOT / "formal/tla/LocalProcessRecovery.tla").read_text()
    config = (ROOT / "formal/tla/LocalProcessRecovery.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for original, replacement, oracle, action in [
        ('=> markerRecorded /\\ markerLock = "launch"', '=> markerLock = "launch"', "GoRequiresDurableParticipant", False),
        ('ELSE IF ~observerKnown THEN "unknown"', 'ELSE IF ~observerKnown THEN "empty"', "EffectsRequireKnownQuiescence", True),
        ('ELSE IF initState \\in {"absent", "exited"} THEN "empty" ELSE "live"', 'ELSE IF ~leaderAlive THEN "empty" ELSE "live"', "EffectsRequireKnownQuiescence", True),
        ('recoveryObservation = "empty"\n  /\\ handQuiescent', 'recoveryObservation = "empty"', "EffectsRequireHandQuiescence", True),
        ('/\\ ~sessionReceipt\n  /\\ handQuiescent', '/\\ ~sessionReceipt', "SessionReceiptRequiresAllParticipants", True),
        ('/\\ (~markerRecorded \\/ (observerKnown /\\ initState \\in {"absent", "exited"}))\n  /\\ sessionReceipt\' = TRUE', '/\\ sessionReceipt\' = TRUE', "SessionReceiptRequiresAllParticipants", True),
        ('CancelCleanup ==\n  /\\ cleanupCaller \\in Waiters\n  /\\ cleanupCaller\' = NoLock /\\ cleanupStage\' = "idle"\n  /\\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars,',
         'CancelCleanup ==\n  /\\ cleanupCaller \\in Waiters\n  /\\ cleanupCaller\' = NoLock /\\ cleanupStage\' = "idle"\n  /\\ markerLock\' = "free"\n  /\\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded, observerKnown, recoveryObservation, rootPresent, resourceEffect, deletion, handQuiescent, sessionReceipt,',
         "CleanupCancellationRetainsParticipant", True),
        ('EnterCleanup(c, operation) ==\n  /\\ markerLock = "remove" /\\ cleanupCaller = NoLock',
         'EnterCleanup(c, operation) ==\n  /\\ markerLock = "remove"', "CleanupBorrowIsExclusive", True),
        ('/\\ RecoveryQuiescent /\\ cleanupFenceLive', '/\\ RecoveryQuiescent', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage \\in {"memory-admit", "memory-next"}\n  /\\ cleanupFenceLive /\\ RecoveryQuiescent', 'cleanupStage \\in {"memory-admit", "memory-next"}\n  /\\ RecoveryQuiescent', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage \\in {"memory-admit", "memory-next"}\n  /\\ cleanupFenceLive /\\ RecoveryQuiescent', 'cleanupStage \\in {"memory-admit", "memory-next"}\n  /\\ (cleanupStage = "memory-next" \\/ cleanupFenceLive) /\\ RecoveryQuiescent', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage = "memory-finish"\n  /\\ cleanupFenceLive /\\ RecoveryQuiescent', 'cleanupStage = "memory-finish"\n  /\\ RecoveryQuiescent', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage = "physical"\n  /\\ cleanupFenceLive /\\ RecoveryQuiescent', 'cleanupStage = "physical"\n  /\\ RecoveryQuiescent', "CleanupEffectRequiresFreshFence", True),
        ('controlQuiescent\' = TRUE /\\ cleanupStage\' = "memory-admit"', 'controlQuiescent\' = FALSE /\\ cleanupStage\' = "memory-admit"', "RootRemovalFollowsParticipants", False),
        ('mountsReleased\' = TRUE /\\ cleanupStage\' = "physical"', 'mountsReleased\' = FALSE /\\ cleanupStage\' = "physical"', "RootRemovalFollowsParticipants", False),
        ('/\\ cleanupFenceLive /\\ ~rootPresent', '/\\ cleanupFenceLive', "RemovedIsPhysical", False),
        ('/\\ cleanupFenceLive /\\ ~rootPresent', '/\\ ~rootPresent', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage = "physical"\n  /\\ cleanupFenceLive /\\ RecoveryQuiescent', 'cleanupStage = "physical"\n  /\\ cleanupFenceLive', "EffectsRequireKnownQuiescence", True),
        # CP1-3 mutate only the production-corresponding next-effect edges.
        # Store responses themselves remain possible after authority loss.
        ('CheckpointStep("checkpoint", "checkpoint-put", TRUE)',
         'CheckpointStep("checkpoint", "checkpoint-put", FALSE)', "CleanupEffectRequiresFreshFence", True),
        ('CheckpointStep("checkpoint-receipt", "checkpoint-delete-ready", TRUE)',
         'CheckpointStep("checkpoint-receipt", "checkpoint-delete-ready", FALSE)', "CleanupEffectRequiresFreshFence", True),
        ('CheckpointStep("checkpoint-delete-ready", "checkpoint-delete", TRUE)',
         'CheckpointStep("checkpoint-delete-ready", "checkpoint-delete", FALSE)', "CleanupEffectRequiresFreshFence", True),
        ('cleanupStage = "checkpoint-finish"\n  /\\ cleanupFenceLive',
         'cleanupStage = "checkpoint-finish"', "CheckpointSuccessRequiresCurrent", True),
        ('CancelCleanup ==\n  /\\ cleanupCaller \\in Waiters\n  /\\ cleanupCaller\' = NoLock /\\ cleanupStage\' = "idle"',
         'CancelCleanup ==\n  /\\ cleanupCaller \\in Waiters\n  /\\ cleanupCaller\' = NoLock /\\ cleanupStage\' = "checkpoint-complete"',
         "CheckpointSuccessRequiresCurrent", True),
    ]:
        if source.count(original) != 1:
            raise SystemExit(f"Local recovery mutation anchor drifted: {oracle}")
        keyword = "PROPERTIES" if action else "INVARIANTS"
        mutations.append((source.replace(original, replacement), header + f"{keyword} {oracle}\n",
                          "action:" + oracle if action else oracle))
    for witness in ("NoCancelledCleanupReached", "NoQueuedCheckpointReached",
                    "NoUnrecordedRemovalReached", "NoLostRemovedReceiptReached",
                    "NoInterMountAdmissionReached", "NoCheckpointSuccessReached",
                    "NoExpiredCheckpointPutResponseReached",
                    "NoExpiredCheckpointDeleteResponseReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "LocalProcessRecovery", source, config, mutations,
                              dependencies=("LocalProcessLifecycle",),
                              heap_mb=1024, timeout_seconds=240)
    print("Local recovery mutation score: 23/23 killed; 8 async cleanup/checkpoint schedules reachable")


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
                  /\\ UNCHANGED <<vars, mounted, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, pendingAck, requestVars>>"""
    mutant = """CopyRecover(w) == /\\ crashed[w]
                  /\\ crashed' = [crashed EXCEPT ![w] = FALSE]
                  /\\ expected' = [expected EXCEPT ![w] = generation]
                  /\\ UNCHANGED <<generation, value, writes, sourcePresent, targetPresent,
                                  targetIdentity, renameCommitted, headPresent, lastAction,
                                  lastBefore, lastAfter, lastPresentBefore, lastPresentAfter,
                                  mounted, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, pendingAck, requestVars>>"""
    if source.count(original) != 1:
        raise SystemExit("Memory copy recovery mutation anchor drifted; review the model")
    # C2: admit a request after an incomplete scan. The exact scan invariant must
    # reject the old missing-root-as-empty behavior; a tool failure cannot pass.
    scan_guard = "    /\\ scanComplete[w]"
    if source.count(scan_guard) != 1:
        raise SystemExit("Memory copy scan mutation anchor drifted; review the model")
    root_guard = "    /\\ rootMatches[w]"
    if source.count(root_guard) != 1:
        raise SystemExit("Memory copy root mutation anchor drifted; review the model")
    # L7: caller cancellation must not erase the request already executing in
    # SQLite. Check the action property, not a false claim that abort undoes SQL.
    crash_original = """CopyCrash(w) == /\\ mounted[w] /\\ ~crashed[w]
                /\\ crashed' = [crashed EXCEPT ![w] = TRUE]
                /\\ pendingAck' = [pendingAck EXCEPT ![w] = FALSE]
                /\\ UNCHANGED <<vars, mounted, materializationBase, scanComplete, lastMutationScan, rootMatches, lastMutationRoot, requestVars>>"""
    crash_mutant = crash_original.replace(
        "/\\ pendingAck'", """/\\ inFlight' = [inFlight EXCEPT ![w] = "none"]
                /\\ pendingAck'"""
    ).replace("lastMutationRoot, requestVars>>", "lastMutationRoot, requestScan, requestRoot>>")
    if source.count(crash_original) != 1:
        raise SystemExit("Memory copy cancellation mutation anchor drifted; review the model")
    mutations = [
        (source.replace(original, mutant), "CopyRecoveryKeepsMaterializationBase"),
        (source.replace(scan_guard, "    /\\ TRUE"),
         "CopyMutationRequiresCompleteScan"),
        (source.replace(root_guard, "    /\\ TRUE"),
         "CopyMutationRequiresOriginalRoot"),
    ]
    config = (ROOT / "formal/tla/MemoryCASCopy.cfg").read_text(encoding="utf-8")
    # MP: removing the commit-time path precondition reintroduces a write after
    # rename even when id/hash are unchanged. The named action property must
    # fail; a parsing error or unrelated failure is never a mutation kill.
    location_guard = "IF ~sourcePresent THEN CopyLocationConflict\n                   ELSE IF"
    if source.count(location_guard) != 1:
        raise SystemExit("Memory copy location mutation anchor drifted; review the model")
    check_tlc_model_mutations(tlc_jar, "MemoryCAS", source, config,
                              [(mutated, config, invariant) for mutated, invariant in mutations]
                              + [(source.replace(crash_original, crash_mutant), config,
                                  "action:CopyCrashKeepsInFlight"),
                                  (source.replace(location_guard, "IF"), config,
                                  "action:CopyUpdateRequiresOriginalLocation")])
    print("Memory copy mutation score: 5/5 killed (frozen base, complete scan, original root, in-flight cancellation, original location)")
    # RC-M: fail the exact invariant/action, not just any nonzero TLC exit.
    retention_config = (ROOT / "formal/tla/MemoryCASRetention.cfg").read_text(encoding="utf-8")
    retention_mutants = []
    for original, replacement, invariant in [
        (r"/\ w \in retSaved /\ w \notin retCancelled",
         r"/\ w \in retSeen /\ w \notin retCancelled",
         "RetAcknowledgementRequiresDurability"),
        (r"/\ w \in retAck /\ w \notin retCancelled",
         r"/\ w \in retSeen /\ w \notin retCancelled",
         "RetDisposalRequiresDurability"),
        (r"/\ UNCHANGED <<copyVars, retSeen, retInFlight, retSaved, retDisposed>>",
         r"/\ retInFlight' = retInFlight \ {w}" + "\n"
         + r"    /\ UNCHANGED <<copyVars, retSeen, retSaved, retDisposed>>",
         "action:RetCancellationPreservesRequest"),
    ]:
        if source.count(original) != 1:
            raise SystemExit(f"Memory retention mutation anchor drifted: {invariant}")
        retention_mutants.append((source.replace(original, replacement), retention_config, invariant))
    check_tlc_model_mutations(tlc_jar, "MemoryCAS", source, retention_config,
                              retention_mutants)
    print("Memory retention mutation score: 3/3 killed (durable ack, disposal, late commit)")


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


def check_realization_publication_model_mutants(tlc_jar: Path) -> None:
    # R1 all Stage receipts republished after the activation switch; R2 exact
    # retry forgets Draining receipts. Each negative control must violate its
    # named safety invariant, never count a parser failure/timeout as a kill.
    # Two witnesses prove recovery and response loss are actually reachable.
    source = (ROOT / "formal/tla/SessionRealizationMutex.tla").read_text()
    config = (ROOT / "formal/tla/SessionRealizationPublication.cfg").read_text()
    header = config.split("INVARIANTS", 1)[0]
    mutations = []
    for anchor, broken, invariant in [
        ("publish |-> PublicationCandidates(@.staged, ActiveGenerations)", "publish |-> @.staged",
         "PublicationExcludesRetiredGenerations"),
        ("(publication[session].publish \\cup publication[session].drain)",
         "publication[session].publish", "LostActivationCanReplay"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Realization publication mutation anchor drifted; review the model")
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    for witness in ("NoRecoveredPublicationReached", "NoLostActivationReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    check_tlc_model_mutations(tlc_jar, "SessionRealizationMutex", source, config, mutations)
    print("Realization publication: 2/2 mutants killed; recovery/response-loss witnesses reachable")


def check_realization_renewal_model_mutant(tlc_jar: Path) -> None:
    check_realization_publication_model_mutants(tlc_jar)
    # S1-S9 extend the existing Worker lifecycle family: startup and withdrawal
    # share the same registration owner as drain, not a second cleanup model.
    source = (ROOT / "formal/tla/WorkerDrain.tla").read_text()
    config = (ROOT / "formal/tla/WorkerDrain.cfg").read_text()
    header = "SPECIFICATION Spec\nCONSTANT MaxClaims = 3\nCHECK_DEADLOCK FALSE\n"
    mutations = []
    for anchor, broken, invariant in [
        ('AcceptProtocol == /\\ startupPhase = "checking_protocol"\n                  /\\ protocolCompatible',
         'AcceptProtocol == /\\ startupPhase = "checking_protocol"', "RegistrationEvidenceRequired"),
        ('/\\ ttlPositive\n                  /\\ startupPhase',
         '/\\ startupPhase', "RegistrationEvidenceRequired"),
        ('RejectProtocol == /\\ startupPhase = "checking_protocol"\n                  /\\ ~(protocolCompatible /\\ ttlPositive)',
         'RejectProtocol == /\\ FALSE', "temporal:InvalidRegistrationTerminates"),
        ('AcceptInitialReady == /\\ startupPhase = "awaiting"',
         'AcceptInitialReady == /\\ startupPhase = "probing"', "StartupAdmissionRequiresProbe"),
        ('BeginWithdrawal == /\\ withdrawal = "none"',
         'BeginWithdrawal == /\\ withdrawalCalls < 2', "OneWithdrawal"),
        ('(IF outcome = "applied" THEN "Dead" ELSE registryState)',
         '"Dead"', "RemoteDeathRequiresReceipt"),
        ('(startupPhase = "failed" \\/ shutdown.cleanupFinished)',
         'shutdown.cleanupFinished', "temporal:StartupFailureTerminates"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Worker startup/withdrawal mutation anchor drifted")
        property_line = ("PROPERTIES " + invariant.removeprefix("temporal:")
                         if invariant.startswith("temporal:") else "INVARIANTS " + invariant)
        mutations.append((source.replace(anchor, broken), header + property_line, invariant))
    for witness in ("NoFailedReturnReached", "NoReadyClaimReached",
                    "NoUnknownWithdrawalReached", "NoPendingHeartbeatCancelled",
                    "NoIncompatibleReturnReached", "NoZeroTtlReturnReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    check_tlc_model_mutations(tlc_jar, "WorkerDrain", source, config, mutations)
    print("Worker startup/withdrawal: 7/7 mutants killed; six lifecycle witnesses reachable")
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
    # Authority A1-A8: remove each decisive guard separately; exact named
    # invariants, not parser/runtime errors, must reject the counterexample.
    # Two inverted reachability witnesses require actual grant and cleanup.
    source = (ROOT / "formal/tla/SessionRealizationAuthority.tla").read_text()
    config = (ROOT / "formal/tla/SessionRealizationAuthority.cfg").read_text()
    anchors = [
        ('IF projection = "Live" /\\ lease = request', 'IF lease = request',
         "ClosedGenerationCannotRenew"),
        ('Grant ==\n  /\\ UNCHANGED selectionVars\n  /\\ projection = "Live"',
         'Grant ==\n  /\\ UNCHANGED selectionVars\n  /\\ TRUE',
         "NoGrantAfterClose"),
        ('admitted == projection = "Closed" /\\ lease = cleanup[c]',
         'admitted == TRUE', "OldCleanupCannotTouchReplacement"),
        ('/\\ slotIdentity = selectedSlot[c]', '/\\ TRUE', "OldCleanupCannotTouchReplacement"),
        ('workerClosed\' = TRUE /\\ peerLive\' = FALSE',
         'workerClosed\' = TRUE /\\ UNCHANGED peerLive', "WorkerCloseIncludesPeer"),
        ('FinishPeer ==\n  /\\ workerClosed', 'FinishPeer ==\n  /\\ ~ownerLocked /\\ workerClosed',
         "temporal:PeerEventuallyCleans"),
    ]
    mutations = []
    for guard, broken, invariant in anchors:
        if source.count(guard) != 1:
            raise SystemExit("Session authority mutation anchor drifted")
        mutations.append((source.replace(guard, broken), config, invariant))
    for invariant in ("NoGrantReached", "NoCleanupReached"):
        mutations.append((source, config + "\nINVARIANT " + invariant + "\n", invariant))
    check_tlc_model_mutations(tlc_jar, "SessionRealizationAuthority", source, config, mutations)
    print("Session authority: 6/6 guard mutants killed; grant/cleanup reachability witnessed")
    # H1/H2 ordinary and panicking heartbeat completion must use the SAME
    # cohort fence. H3 the wrapper cannot finish ahead of revocation. Exact
    # semantic oracles, plus panic/completion reachability, exclude inert proofs.
    source = (ROOT / "formal/tla/WorkerAuthorityExit.tla").read_text()
    config = (ROOT / "formal/tla/WorkerAuthorityExit.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for anchor, broken, invariant in [
        ("  /\\ CloseWorker\n", '  /\\ IF kind = "panicked" THEN UNCHANGED vars ELSE CloseWorker\n',
         "HeartbeatExitFencesCohort"),
        ("  /\\ CloseWorker\n", '  /\\ IF kind = "returned" THEN UNCHANGED vars ELSE CloseWorker\n',
         "HeartbeatExitFencesCohort"),
        ('/\\ exitPhase = "revoking" /\\ cleaned /\\ peerCleaned /\\ ~ownerLocked',
         '/\\ exitPhase = "revoking"', "CompletionRequiresRevocation"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Worker authority exit mutation anchor drifted")
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    for witness in ("NoPanickedRevocationReached", "NoCompletedRevocationReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    check_tlc_model_mutations(tlc_jar, "WorkerAuthorityExit", source, config, mutations,
                              dependencies=("SessionRealizationAuthority",))
    print("Worker authority exit: 3/3 mutants killed; panic/completion reachability witnessed")
    # W1-W11 and A1-A6: one root proof, request-start latency, irreversible
    # closure and atomic Session installation. Each removed guard must produce
    # the exact semantic counterexample; admission/renewal witnesses exclude
    # configurations that deny every useful action.
    source = (ROOT / "formal/tla/WorkerExecutionAuthority.tla").read_text()
    config = (ROOT / "formal/tla/WorkerExecutionAuthority.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for anchor, broken, invariant in [
        ('(rootPhase = "Starting" \\/ RootCurrent)',
         '(rootPhase # "Closed")', "ReceiptNeverRevivesExpiredProof"),
        ('(rootPhase = "Starting" \\/ RootCurrent)',
         'TRUE', "ClosedRootNeverReopens"),
        ("/\\ observed < heartbeatRequest + 2", "/\\ TRUE",
         "ReceiptNeverRevivesExpiredProof"),
        ("/\\ observed >= heartbeatRequest", "/\\ TRUE",
         "ReceiptNeverRevivesExpiredProof"),
        ("InstallReplacement ==\n  /\\ RootCurrent /\\ Replace",
         "InstallReplacement ==\n  /\\ Replace", "SessionInstallationRequiresCurrentRoot"),
        ("FreshAdmission ==\n  /\\ RootCurrent /\\ admissions < 2",
         "FreshAdmission ==\n  /\\ admissions < 2", "SessionInstallationRequiresCurrentRoot"),
        ("RenewProjection ==\n  /\\ RootCurrent /\\ ApplyRenewal",
         "RenewProjection ==\n  /\\ ApplyRenewal", "SessionInstallationRequiresCurrentRoot"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit("Worker execution authority mutation anchor drifted")
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    for witness in ("NoAdmissionReached", "NoRenewedRootReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    # The negative elapsed branch must be reachable, not merely a guard over
    # impossible inputs. NoRegressionResponse is an action property, so the
    # existing oracle demands its named temporal counterexample.
    mutations.append((source, header + "PROPERTY NoRegressionResponse", "action:NoRegressionResponse"))
    check_tlc_model_mutations(tlc_jar, "WorkerExecutionAuthority", source, config, mutations,
                              dependencies=("SessionRealizationAuthority",))
    print("Worker execution authority: 7/7 mutants killed; admission/renewal/regression reachable")
    check_session_execution_binding_model_mutants(tlc_jar)
    check_execution_replacement_model_mutants(tlc_jar)


def check_session_execution_binding_model_mutants(tlc_jar: Path) -> None:
    # S1-S21: independently remove each authority/identity guard; release the
    # original mutex between check and effect; permit a late weak upgrade or
    # rebinding. The receipt oracle is independent of the mutated predicate.
    source = (ROOT / "formal/tla/SessionExecutionBinding.tla").read_text()
    config = (ROOT / "formal/tla/SessionExecutionBinding.cfg").read_text()
    header = "SPECIFICATION BindingSpec\nCHECK_DEADLOCK FALSE\n"
    mutations = []
    for anchor, broken, invariant in [
        ('CurrentBinding == /\\ ExecutionRootCurrent', 'CurrentBinding == /\\ TRUE', "OnlyCurrentBindingCommits"),
        ('/\\ projection = "Live" /\\ ~sessionExpired', '/\\ TRUE /\\ ~sessionExpired', "OnlyCurrentBindingCommits"),
        ('/\\ ~sessionExpired\n  /\\ boundSlot', '/\\ TRUE\n  /\\ boundSlot', "OnlyCurrentBindingCommits"),
        ('/\\ boundSlot = slotIdentity', '/\\ TRUE', "OnlyCurrentBindingCommits"),
        ('/\\ boundGeneration = lease[1]', '/\\ TRUE', "OnlyCurrentBindingCommits"),
        ('/\\ boundEnvironment = environmentIdentity', '/\\ TRUE', "OnlyCurrentBindingCommits"),
        ('/\\ environmentPhase \\in {"candidate", "resident"}))\n\nSelect', '/\\ TRUE))\n\nSelect', "OnlyCurrentBindingCommits"),
        ('/\\ ~effectHeld /\\ UNCHANGED bindingVars', '/\\ UNCHANGED bindingVars', "OnlyCurrentBindingCommits"),
        ('Select == /\\ selection = "unset"', 'Select == /\\ selection \\in {"unset", "weak"}', "WeakExecutionCannotUpgrade"),
        ('/\\ boundSlot = 0 /\\ projection', '/\\ projection', "BindingIsWriteOnce"),
        ('/\\ deploymentReady /\\ selection\' = "scoped"', '/\\ selection\' = "scoped"', "OnlyProbedDeploymentSelected"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Session execution binding mutation anchor drifted: {anchor}")
        property_kind = "PROPERTY" if invariant == "BindingIsWriteOnce" else "INVARIANTS"
        mutations.append((source.replace(anchor, broken), header + property_kind + " " + invariant,
                          "action:" + invariant if property_kind == "PROPERTY" else invariant))
    # P1-P6 bypass each shared guard ONLY for a pre-Environment provider
    # callback. Execution-mode counterexamples cannot kill these mutants;
    # this prevents the new branch from borrowing the old branch's proof.
    for anchor, broken in [
        ('CurrentBinding == /\\ ExecutionRootCurrent', 'CurrentBinding == /\\ (providerSource \\/ ExecutionRootCurrent)'),
        ('/\\ projection = "Live" /\\ ~sessionExpired', '/\\ (providerSource \\/ projection = "Live") /\\ ~sessionExpired'),
        ('/\\ ~sessionExpired\n  /\\ boundSlot', '/\\ (providerSource \\/ ~sessionExpired)\n  /\\ boundSlot'),
        ('/\\ boundSlot = slotIdentity', '/\\ (providerSource \\/ boundSlot = slotIdentity)'),
        ('/\\ boundGeneration = lease[1]', '/\\ (providerSource \\/ boundGeneration = lease[1])'),
        ('/\\ ~effectHeld /\\ UNCHANGED bindingVars', '/\\ (providerSource \\/ ~effectHeld) /\\ UNCHANGED bindingVars'),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Provider fence source mutation anchor drifted: {anchor}")
        invariant = "OnlyCurrentBindingCommits"
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    # M1-M3: a queued request cannot commit before acquiring the mount lock;
    # a cached Copy ack cannot skip current authority; replay cannot redrain.
    for anchor, broken, invariant in [
        ('/\\ memoryWait # 1\n', '/\\ TRUE\n', "OnlyCurrentBindingCommits"),
        ('/\\ effects < 2 /\\ CurrentBinding', '/\\ effects < 2 /\\ (memoryDrains = 1 \\/ CurrentBinding)', "OnlyCurrentBindingCommits"),
        ('IF memoryWait = 2 THEN 1 ELSE memoryDrains', 'IF memoryWait = 2 THEN memoryDrains + 1 ELSE memoryDrains', "MemoryCopyDrainedOnce"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Memory acknowledgement mutation anchor drifted: {anchor}")
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    # D1-D3: pending preparation cannot report an effect; a completed wait and
    # a cached Memory ack cannot bypass the shared current authority. The
    # independent receipt oracle is unchanged, including zero-Memory paths.
    for anchor, broken in [
        ('/\\ providerWait # 1\n', '/\\ TRUE\n'),
        ('/\\ effects < 2 /\\ CurrentBinding', '/\\ effects < 2 /\\ (providerWait = 2 \\/ CurrentBinding)'),
        ('/\\ effects < 2 /\\ CurrentBinding', '/\\ effects < 2 /\\ ((providerWait = 2 /\\ memoryDrains = 1) \\/ CurrentBinding)'),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Disposal preparation mutation anchor drifted: {anchor}")
        invariant = "OnlyCurrentBindingCommits"
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    # One constructor for each phase-only pending-response,
    # post-response-authority and exact-retry mutant. Another phase cannot
    # supply its counterexample; retry requires an actual prior phase effect.
    # Compensation and backend cancellation remain adapter obligations.
    for phase, selector in (("disposal", "physicalDisposal"),
                            ("reconstruction", "coldReconstruction"),
                            ("restore-disposal", "restoredDisposal"),
                            ("checkpoint", "checkpointUpload"), ("adoption", "liveAdoption"),
                            ("creation", "localCreation")):
        for anchor, broken in [
            ('/\\ providerWait # 1\n', '/\\ (' + selector + ' \\/ providerWait # 1)\n'),
            ('/\\ effects < 2 /\\ CurrentBinding', '/\\ effects < 2 /\\ ((' + selector + ' /\\ providerWait = 2) \\/ CurrentBinding)'),
            ('/\\ effects < 2 /\\ CurrentBinding', '/\\ effects < 2 /\\ ((' + selector + ' /\\ "' + phase + '" \\in effectPhases) \\/ CurrentBinding)'),
        ]:
            if source.count(anchor) != 1:
                raise SystemExit(f"{phase} mutation anchor drifted: {anchor}")
            invariant = "OnlyCurrentBindingCommits"
            mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    # CT1-CT6: fix a redundant Provider clock in its rejecting class, separately
    # for each ordinary phase (restored disposal shares disposal). The positive
    # oracle expands current-authority coordinates independently of the mutant;
    # a safety-only check could otherwise pass by denying every fresh effect.
    anchor = 'CheckEffect == /\\ ~effectHeld /\\ effects < 2 /\\ CurrentBinding'
    if source.count(anchor) != 1:
        raise SystemExit("Provider clock over-denial mutation anchor drifted")
    for phase in ("preparation", "disposal", "reconstruction", "checkpoint", "adoption", "creation"):
        broken = anchor + '\n  /\\ ~(providerSource /\\ providerPhase = "' + phase + '")'
        invariant = "FreshProviderEffectRemainsEnabled"
        mutations.append((source.replace(anchor, broken), header + "INVARIANTS " + invariant, invariant))
    # Both topology paths must reach actual effects. Otherwise an accidental
    # mandatory Worker guard could make all direct-local safety checks vacuous.
    for witness in ("NoCandidateEffectReached", "NoResidentEffectReached",
                    "NoLocalEffectReached", "NoRegisteredEffectReached",
                    "NoProviderEffectReached", "NoUnreadyProviderEffectReached",
                    "NoMemoryAckReached", "NoMemoryReplayReached",
                    "NoEmptyPreparationReached", "NoCachedPreparationReached",
                    "NoPhysicalDisposalReached", "NoCachedDisposalReached",
                    "NoColdReconstructionReached", "NoColdReconstructionReplayReached",
                    "NoRestoredDisposalReached", "NoRestoredDisposalReplayReached",
                    "NoCheckpointUploadReached", "NoCheckpointReplayReached",
                    "NoLiveAdoptionReached", "NoLiveAdoptionReplayReached",
                    "NoLocalCreationReached", "NoLocalCreationReplayReached"):
        mutations.append((source, header + "INVARIANTS " + witness, witness))
    check_tlc_model_mutations(tlc_jar, "SessionExecutionBinding", source, config, mutations,
                              dependencies=("SessionRealizationAuthority", "WorkerExecutionAuthority"),
                              # Seven phase classes retain the same complete
                              # two-effect state space; raise tool resources,
                              # never truncate exploration to fit the old budget.
                              heap_mb=4096, timeout_seconds=600)
    print("Session execution binding: 47/47 mutants killed; 22 witnesses cover execution, provider projection, Memory, preparation, physical/restored disposal, cold reconstruction, live adoption, local creation and checkpoint upload/replay; fresh provider admission cannot be suppressed by a second clock")


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




def check_sandbox_control_model_mutants(tlc_jar: Path) -> None:
    # SC1 first close forgets participant; SC2 cancellation forgets it; SC3
    # return skips join; SC4 Drop releases early; SC5 late start after cancel;
    # SC6 join before exit; SC7 disable completion. Each uses the exact oracle
    # and green baseline; four witnesses exclude unreachable race/terminal paths.
    source = (ROOT / "formal/tla/SandboxControlPublication.tla").read_text()
    config = (ROOT / "formal/tla/SandboxControlPublication.cfg").read_text()
    header = config.split("INVARIANTS\n", 1)[0]
    mutations = []
    for anchor, broken, oracle, kind in [
        ('retained\' = (phase = "Active")', "retained' = FALSE", "ReturnRequiresJoin", "INVARIANTS"),
        ('closers\' = [closers EXCEPT ![i] = "Cancelled"]\n    /\\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,\n                   task, cancelled, retained>>',
         'closers\' = [closers EXCEPT ![i] = "Cancelled"]\n    /\\ retained\' = FALSE\n    /\\ UNCHANGED <<phase, nextLease, activeLease, staleLease, closedOnce,\n                   task, cancelled>>',
         "ReturnRequiresJoin", "INVARIANTS"),
        ('/\\ (~retained \\/ task \\in Quiescent)', '/\\ TRUE', "ReturnRequiresJoin", "INVARIANTS"),
        ('/\\ task \\in Quiescent\n    /\\ phase\' = "Vacant"', '/\\ phase\' = "Vacant"',
         "ReleaseRequiresJoin", "INVARIANTS"),
        ('phase = "Active" /\\ ~cancelled /\\ task = "Absent"',
         'phase = "Active" /\\ task = "Absent"', "NoStartAfterCancellation", "PROPERTIES"),
        ('JoinTask ==\n    /\\ task = "Exited"', 'JoinTask ==\n    /\\ task \\in {"Running", "Exited"}',
         "JoinRequiresExit", "PROPERTIES"),
        ('FinishClose(i) ==\n    /\\ closers[i] = "Waiting"', 'FinishClose(i) ==\n    /\\ FALSE\n    /\\ closers[i] = "Waiting"',
         "temporal:DisposalEventuallyReturns", "PROPERTIES"),
    ]:
        if source.count(anchor) != 1:
            raise SystemExit(f"Sandbox control mutation anchor drifted: {oracle}")
        selected = oracle.removeprefix("temporal:")
        mutations.append((source.replace(anchor, broken), header + f"{kind} {selected}\n",
                          "action:" + oracle if kind == "PROPERTIES" and not oracle.startswith("temporal:") else oracle))
    for witness in ("NoConcurrentWaitReached", "NoCancelledPendingReached",
                    "NoFinishedDisposalReached", "NoDroppedPendingReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "SandboxControlPublication", source, config, mutations)
    print("Sandbox control mutation score: 7/7 killed; 4 disposal/drop schedules reachable")


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

    # I1 anchoring without an observed input invents committed truth; I2 an
    # input-only observation cannot release the Run activity. These mutations
    # exercise the same kernel consumed by the Event and Start compositions.
    kernel = (ROOT / "formal/tla/SessionEventKernel.tla").read_text()
    kernel_config = "\n".join(line for line in config.splitlines()
        if not any(word in line for word in ("ObservationTimes =", "UnknownTime =", "PROPERTIES First")))
    guard = 'AnchorCommittedInput ==\n    /\\ kEventPhase = "Activated"\n    /\\ kCommittedDisposition = "Running"'
    assert kernel.count(guard) == 1
    unproven = kernel.replace(guard, 'AnchorCommittedInput ==\n    /\\ kEventPhase = "Activated"')
    release = 'SettleRunActivity ==\n    /\\ kEventPhase = "Anchored"'
    assert kernel.count(release) == 1
    premature = kernel.replace(release, 'SettleRunActivity ==\n    /\\ kEventPhase \\in {"Activated", "Anchored"}')
    check_tlc_model_mutations(tlc_jar, "SessionEventKernel", kernel, kernel_config,
        [(unproven, kernel_config, "Safety"), (premature, kernel_config, "Safety")],
        dependencies=("SessionActivityKernel", "RunIngressKernel"))

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

    # Publication partition and lost-response mutations belong to the same
    # driver's check_realization_publication_model_mutants, already invoked by
    # the strict renewal gate. Keep one owner instead of a second coarse graph.
    # S1-S3: a surviving resident Arc cannot bypass the physical observation.
    # Restore the old caller bypass in the existing owner model, then require
    # its named routing invariant to fail (not a parser/tool/timeout failure).
    projection = (ROOT / "formal/tla/SessionEnvironmentProjection.tla").read_text()
    config = (ROOT / "formal/tla/SessionEnvironmentProjection.cfg").read_text()
    for old, new in [("{effect_a, effect_b}", "{effect_a}"),
                     ("{generation_1, generation_2}", "{generation_1}"),
                     ("{binding_1, binding_2}", "{binding_1}")]:
        config = config.replace(old, new)
    config = config.replace("    Safety", "    ResidentSynchronizationObservesPhysicalOwner")
    anchor = "LET observed == TRUE"
    assert projection.count(anchor) == 1
    check_tlc_model_mutations(tlc_jar, "SessionEnvironmentProjection", projection, config,
        [(projection.replace(anchor, "LET observed == FALSE"), config,
          "ResidentSynchronizationObservesPhysicalOwner")])
    convergence = (ROOT / "formal/tla/SessionEnvironmentConvergence.cfg").read_text()
    # R1 no root ordering admits late A; R2 treating a descriptor as a physical
    # owner strands the Coordinator; R3 bypassing real ownership publishes B
    # over a retained A. Only named semantic counterexamples count as kills.
    order = "incoming.revision > before.revision \\/ incoming = before"
    compatible = "PhysicalProjectionCompatible(physical, incoming)"
    reject_descriptor = projection.replace("                       " + compatible,
        "                       " + compatible + ' /\\ physical # "none"', 1)
    bypass_owner = projection.replace("                       " + compatible,
                                      "                       TRUE", 1)
    check_tlc_model_mutations(tlc_jar, "SessionEnvironmentProjection", projection, convergence, [
        (projection.replace(order, "TRUE", 1), convergence, "ProjectionConvergenceSafety"),
        (reject_descriptor, convergence, "temporal:ProjectionEventuallyConverges"),
        (bypass_owner, convergence, "ProjectionConvergenceSafety"),
    ])
    print("Session observation model mutations passed, including resident physical observation; publication uses the canonical renewal gate")


def check_observation_trace_mutants(tlc_jar: Path, trace_dir: Path) -> None:
    from render_runtime_refinement_traces import render_observations, module_name
    manifest = json.loads((ROOT / "formal/runtime-refinement-manifest.json").read_text())
    # X1 bypass binding stamping; X2 resample processed time; X3 publish retired;
    # X4 rewrite recovered metadata. Mutate REAL emitted observations, not the
    # model. Each must violate the exact relation against a green trace baseline.
    for name in ["binding-apply", "processed-first-replay-terminal", "publication-cardinality-renewal", "store-torn-prefix", "environment-projection-convergence"]:
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
            elif family == "environment-projection":
                step = next(s for s in document["steps"] if s["rule"] == "p0-stale")
                step["after"]["binding"] = "A"
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
    print("Production observation trace mutation score: 5/5 killed")

def check_cowork_lifecycle_model_mutants(tlc_jar: Path) -> None:
    # Worker fault matrix W1 registry-only renewal; W2 evidence-only renewal;
    # W3 delayed probe success. Removing either expiry lane or erasing request
    # latency must violate its exact invariant; first establish the green model.
    source = (ROOT / "formal/tla/WorkerAuthorityEvidence.tla").read_text()
    config = (ROOT / "formal/tla/WorkerAuthorityEvidence.cfg").read_text()
    expiry = "(registryAge' < Window /\\ evidenceAge' < Window)"
    check_tlc_model_mutations(tlc_jar, "WorkerAuthorityEvidence", source, config, [
        (source.replace(expiry, "(registryAge' < Window)", 1), config, "BothProofsRequired"),
        (source.replace(expiry, "(evidenceAge' < Window)", 1), config, "BothProofsRequired"),
        (source.replace("evidenceAge' = probeAge", "evidenceAge' = 0", 1), config, "ProbeLatencyIncluded"),
    ])
    # W4 drain request + fair completion/ack progress => acknowledgement.
    # Reopening admission after acknowledgement must violate the existing fence.
    # Composition table: W4a reopening -> safety violation; W4b no ACK action
    # -> liveness violation even with startup/withdrawal fairness; W4c a valid
    # registration can reach ACK. The positive baseline checks both families.
    source = (ROOT / "formal/tla/WorkerDrain.tla").read_text()
    config = (ROOT / "formal/tla/WorkerDrain.cfg").read_text()
    check_tlc_model_mutations(tlc_jar, "WorkerDrain", source, config, [
        (source.replace('RepeatDrain == /\\ drainRequested /\\ gate = "Closed"\n               /\\ UNCHANGED vars',
                        'RepeatDrain == /\\ drainRequested /\\ gate = "Closed"\n'
                        '               /\\ gate\' = "Open"\n'
                        '               /\\ UNCHANGED <<claimInProgress, claimAttempts, claims, drainRequested, registryState, drainAcknowledged, attemptsAtRegistryDrain, claimsAtDrain, shutdown>>'),
         config, "RegistryDrainFollowsLocalFence"),
        (source.replace('AcknowledgeDrain == /\\ registryState = "Draining"',
                        'AcknowledgeDrain == /\\ FALSE /\\ registryState = "Draining"'),
         "SPECIFICATION ProgressSpec\nCONSTANT MaxClaims = 3\n"
         "PROPERTIES DrainEventuallyAcknowledged\n",
         "temporal:DrainEventuallyAcknowledged"),
        (source, "SPECIFICATION ProgressSpec\nCONSTANT MaxClaims = 3\n"
                 "INVARIANTS NoDrainAcknowledgedReached\n",
         "NoDrainAcknowledgedReached"),
        # G2 old normal-shutdown bug: abandon active work after the grace period
        # without requesting cancellation or retaining the bounded observer.
        (source.replace('(shutdown.cancelRequested /\\ shutdown.cleanupExpired)',
                        'shutdown.graceExpired', 1),
         config, "ShutdownDoesNotDiscardUncancelledWork"),
        # G3 cancellation/timeout is not evidence of physical return.
        (source.replace('!.quiesced = (shutdown.active = {})', '!.quiesced = TRUE', 1),
         config, "NoFabricatedQuiescence"),
    ])
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
        (source.replace('/\\ wireOutcome = "Active" /\\ downlink > 0', '/\\ wireOutcome = "Active" /\\ downlink > 0 /\\ hostLeft = 0'), config.replace("INVARIANTS Safety", "INVARIANTS WireCanAdvance Safety"), "WireCanAdvance"),
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
    # R2/R4/R7/R8: logical termination, partial source preparation and partial
    # physical cleanup cannot become a full cleanup receipt. Each mutant must
    # violate the named evidence invariant, not merely fail parsing or tooling.
    source = (ROOT / "formal/tla/SessionDeletion.tla").read_text()
    config = (ROOT / "formal/tla/SessionDeletion.cfg").read_text().replace(
        "    DeleteSafety", "    PhysicalEvidenceRequired\n    DeleteSafety")
    prepare = source[source.index("PrepareParticipant(participant, effect) =="):source.index("RemoveTarget(target, effect) ==")]
    remove = source[source.index("RemoveTarget(target, effect) =="):source.index("Next ==")]
    success = source[source.index("CleanupSucceeds =="):source.index("CommitTombstone ==")]
    check_tlc_model_mutations(tlc_jar, "SessionDeletion", source, config, [
        (source.replace(prepare, prepare.replace("~physicalActive /\\ effect", "effect", 1)),
         config, "PhysicalEvidenceRequired"),
        (source.replace(remove, remove.replace("~physicalActive /\\ prepared = Participants", "TRUE", 1)),
         config, "PhysicalEvidenceRequired"),
        (source.replace(success, success.replace("removedTargets = PhysicalTargets", "removedTargets # {}", 1)),
         config, "PhysicalEvidenceRequired"),
        (source.replace(success, success.replace("/\\ ~physicalActive /\\ prepared = Participants\n                         /\\ removedTargets = PhysicalTargets", "", 1)),
         config, "PhysicalEvidenceRequired"),
    ], dependencies=("SessionCleanupKernel",))
    # P0: current errors stay visible under active aggregate authority; neither
    # error presentation nor a resolving tool reply can authorize new input.
    source = (ROOT / "formal/tla/SessionRuntimeProjection.tla").read_text()
    config = (ROOT / "formal/tla/SessionRuntimeProjection.cfg").read_text().replace(
        "    Safety", "    ErrorIsVisibleWithoutRelease\n    ActiveAggregateNeverAdmits\n    Safety")
    check_tlc_model_mutations(tlc_jar, "SessionRuntimeProjection", source, config, [
        (source.replace('ELSE IF phase = "error" THEN "error"',
                        'ELSE IF phase = "error" THEN aggregate', 1),
         config, "ErrorIsVisibleWithoutRelease"),
        (source.replace('CanSend == aggregate = "idle" /\\ phase',
                        'CanSend == phase', 1),
         config, "ActiveAggregateNeverAdmits"),
    ])
    print("Cowork lifecycle mutation score: 18/18 killed (worker evidence 3, drain 4, ACP/composition/rollout 5, cleanup evidence 4, presentation/admission 2); drain ACK witness reachable")


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


def check_checkpoint_recovery_model_mutants(tlc_jar: Path) -> None:
    # Cause/effect rules match execution/checkpoint_observation.rs: a failed,
    # foreign or interrupted observation cannot become fresh execution; blocked
    # observation cannot consume evidence; valid reads still resume. Require a
    # green baseline and exact failures/reachability, never parse errors/timeouts.
    source = (ROOT / "formal/tla/CheckpointRecovery.tla").read_text()
    config = (ROOT / "formal/tla/CheckpointRecovery.cfg").read_text()
    header = "SPECIFICATION Spec\nCONSTANT MaxChunks = 3\n"
    mutations = []
    anchor = 'Recover == /\\ phase = "crashed"\n           /\\ readStatus \\in AcceptedReads'
    if source.count(anchor) != 1:
        raise SystemExit("checkpoint observation mutation anchor drifted")
    for outcome in ("storage", "fenced", "wrong-run", "wrong-thread", "pending", "cancelled", "deadline"):
        broken = anchor.replace("AcceptedReads", f'AcceptedReads \\cup {{"{outcome}"}}')
        mutations.append((source.replace(anchor, broken),
                          header + "INVARIANTS UnknownObservationNeverExecutes\n",
                          "UnknownObservationNeverExecutes"))
    anchor = 'RejectRead == /\\ phase = "crashed"'
    if source.count(anchor) != 1:
        raise SystemExit("checkpoint rejection mutation anchor drifted")
    # Keep the assignment well formed: mutate only RejectRead's UNCHANGED tuple.
    reject = source[source.index(anchor):source.index("\nComplete ==", source.index(anchor))]
    broken = reject.replace('              /\\ UNCHANGED',
                            "              /\\ checkpointValid' = FALSE\n              /\\ UNCHANGED")
    broken = broken.replace("staged, checkpoint, checkpointValid,", "staged, checkpoint,")
    mutations.append((source.replace(reject, broken),
                      header + "INVARIANTS BlockedPreservesCheckpoint\n", "BlockedPreservesCheckpoint"))
    mutations.append((source.replace(anchor, anchor + " /\\ FALSE"), header +
                      "CHECK_DEADLOCK FALSE\nPROPERTIES ObservationSettles\n", "temporal:ObservationSettles"))
    for anchor, broken, invariant in [
        ('/\\ EvidencePermits /\\ recovery.entry \\in EffectEntries',
         '/\\ recovery.entry \\in EffectEntries', "UnknownPredecessorNeverExecutes"),
        ('/\\ readStatus \\in AcceptedReads /\\ EvidencePermits',
         '/\\ readStatus \\in AcceptedReads', "ControlReturnRequiresEvidence"),
        ('(recovery.evidence = "absent" /\\ ~savedValid)',
         '(recovery.evidence = "absent")', "RecoveredPartialRequiresEvidence"),
    ]:
        if source.count(anchor) != 1: raise SystemExit("entry evidence mutation anchor drifted")
        mutations.append((source.replace(anchor, broken), header + f"INVARIANTS {invariant}\n", invariant))
    broken = reject.replace('              /\\ UNCHANGED',
                            '              /\\ recovery\' = [recovery EXCEPT !.quiescent = TRUE]\n              /\\ UNCHANGED')
    broken = broken.replace("savedValid, recovery>>", "savedValid>>")
    mutations.append((source.replace(reject, broken),
                      header + "INVARIANTS UncertainOutcomeVetoesPhysicalReturn\n", "UncertainOutcomeVetoesPhysicalReturn"))
    mutations.append((source.replace('ReadDeadline == ObserveRead("deadline")', 'ReadDeadline == FALSE'),
                      header + "CHECK_DEADLOCK FALSE\nPROPERTIES RecoveryReadSettles\n", "temporal:RecoveryReadSettles"))
    mutations.append((source.replace('phase = "controlled" /\\ ~recovery.acknowledged', 'phase \\in {"crashed", "blocked", "controlled"} /\\ ~recovery.acknowledged'),
                      header + "INVARIANTS ControlAckRequiresObservedEvidence\n", "ControlAckRequiresObservedEvidence"))
    for witness in ("NoExactResume", "NoAbsentResume", "NoStorageBlock", "NoFenceBlock", "NoDeadlineReadBlock",
                    "NoRunMismatchBlock", "NoThreadMismatchBlock", "NoCancelledReadBlock",
                    "NoUnknownResumeBlock", "NoUnknownHandoffBlock", "NoUnknownCancelBlock",
                    "NoUnknownStopBlock", "NoKnownControlReturn", "NoControlAckReached"):
        mutations.append((source, header + f"INVARIANTS {witness}\n", witness))
    check_tlc_model_mutations(tlc_jar, "CheckpointRecovery", source, config, mutations)
    print("Checkpoint recovery: 15/15 mutants killed; fourteen entry/observation witnesses reachable")




if __name__ == "__main__":
    main()
