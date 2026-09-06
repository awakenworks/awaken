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
    model_selection.add_argument("--resolution-failure-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--config-cas-model-mutant", type=Path, metavar="TLC_JAR")
    model_selection.add_argument("--background-completion-model-mutant", type=Path, metavar="TLC_JAR")
    arguments = parser.parse_args()
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


def check_tlc_model_mutations(tlc_jar: Path, name: str, source: str, config: str,
                              mutations: list[tuple[str, str, str]], *,
                              dependencies: tuple[str, ...] = ()) -> None:
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
            result = subprocess.run(
                ["java", "-Xmx512m", "-jar", str(tlc_jar.resolve()),
                 "-metadir", str(directory / "states"), "-config", str(config_path), str(model)],
                cwd=directory, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                timeout=30, check=False,
            )
            expected_code = 0 if invariant is None else 12
            expected_text = ("Model checking completed. No error has been found." if invariant is None
                             else f"Invariant {invariant} is violated")
            if result.returncode != expected_code or expected_text not in result.stdout:
                raise SystemExit(f"{name} {'baseline failed' if invariant is None else 'mutant was not killed correctly'}:\n{result.stdout}")


if __name__ == "__main__":
    main()
