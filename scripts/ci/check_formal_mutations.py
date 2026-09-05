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
import tempfile


ROOT = Path(__file__).resolve().parents[2]


def prepare_case(case_root: Path) -> None:
    (case_root / "scripts" / "ci").mkdir(parents=True)
    (case_root / "formal").mkdir()
    for name in (
        "check_feature_coverage.py",
        "check_formal_coverage.py",
        "check_proof_boundaries.py",
    ):
        shutil.copy2(ROOT / "scripts" / "ci" / name, case_root / "scripts" / "ci" / name)
    shutil.copy2(ROOT / "scripts" / "ci" / "check_formal.sh", case_root / "scripts" / "ci" / "check_formal.sh")
    for name in (
        "assumptions.json",
        "coverage.json",
        "features.json",
        "proof-boundaries.json",
        "surface-classifications.json",
        "README.md",
    ):
        shutil.copy2(ROOT / "formal" / name, case_root / "formal" / name)
    os.symlink(ROOT / "crates", case_root / "crates", target_is_directory=True)

    # Evidence paths outside crates/formal are checked for existence.  Symlinks
    # keep the mutation cases cheap while leaving the source tree read-only.
    for name in ("docs", "e2e"):
        source = ROOT / name
        if source.exists():
            os.symlink(source, case_root / name, target_is_directory=True)


def must_fail(case_root: Path, checker: str, label: str, *arguments: str) -> None:
    result = subprocess.run(
        ["python3", str(case_root / "scripts" / "ci" / checker), *arguments],
        cwd=case_root,
        text=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if result.returncode == 0:
        raise SystemExit(f"formal mutation survived: {label}")


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
    must_fail(case_root, "check_formal_coverage.py", "removed Kani harness from strict CI")


def mutate_downgraded_claim(case_root: Path) -> None:
    path = case_root / "formal" / "coverage.json"
    ledger = json.loads(path.read_text())
    row = next(item for item in ledger["obligations"] if item["status"] == "kernel_proved")
    row["status"] = "executable_only"
    row.pop("proof_harnesses", None)
    write_json(path, ledger)
    must_fail(case_root, "check_formal_coverage.py", "downgraded proved obligation")


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
    must_fail(case_root, "check_formal_coverage.py", "formal model removed from strict CI")


def mutate_missing_proof_grade(case_root: Path) -> None:
    path = case_root / "formal" / "coverage.json"
    ledger = json.loads(path.read_text())
    ledger["obligations"][0].pop("proof_grade")
    write_json(path, ledger)
    must_fail(case_root, "check_formal_coverage.py", "proof grade removed")


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
    )


def mutate_missing_boundary(case_root: Path) -> None:
    path = case_root / "formal" / "proof-boundaries.json"
    ledger = json.loads(path.read_text())
    ledger["boundaries"].pop()
    write_json(path, ledger)
    must_fail(case_root, "check_proof_boundaries.py", "missing residual boundary")


def mutate_invented_boundary(case_root: Path) -> None:
    path = case_root / "formal" / "proof-boundaries.json"
    ledger = json.loads(path.read_text())
    mutant = dict(ledger["boundaries"][0])
    mutant["requirement_id"] = "invented.requirement.boundary"
    ledger["boundaries"].append(mutant)
    write_json(path, ledger)
    must_fail(case_root, "check_proof_boundaries.py", "invented residual boundary")


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
    )


def mutate_disconnected_product_feature(case_root: Path) -> None:
    path = case_root / "formal" / "features.json"
    ledger = json.loads(path.read_text())
    target = "runtime.parallel_tool_conflicts"
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
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--memory-copy-model-mutant", type=Path, metavar="TLC_JAR")
    arguments = parser.parse_args()
    if arguments.memory_copy_model_mutant is not None:
        check_memory_copy_model_mutant(arguments.memory_copy_model_mutant)
        return
    # Cause/effect decision table: removing a strict-CI proof, downgrading a
    # claim, detaching a model, omitting/inventing a residual boundary,
    # inventing a production proof grade, deleting a proof grade or TCB
    # fail-closed contract, orphaning a formal obligation, reopening an external
    # assumption, or disconnecting a product feature from every end-to-end
    # assurance journey must each make its existing authoritative gate fail. No
    # mutant may introduce a second ledger or a test-only acceptance path.
    mutations = (
        mutate_removed_harness,
        mutate_downgraded_claim,
        mutate_unchecked_model,
        mutate_missing_proof_grade,
        mutate_false_production_grade,
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
            mutation(case_root)
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
                  /\\ UNCHANGED <<vars, mounted, materializationBase, scanComplete, lastMutationScan>>"""
    mutant = """CopyRecover(w) == /\\ crashed[w]
                  /\\ crashed' = [crashed EXCEPT ![w] = FALSE]
                  /\\ expected' = [expected EXCEPT ![w] = generation]
                  /\\ UNCHANGED <<generation, value, writes, sourcePresent, targetPresent,
                                  targetIdentity, renameCommitted, headPresent, lastAction,
                                  lastBefore, lastAfter, lastPresentBefore, lastPresentAfter,
                                  mounted, materializationBase, scanComplete, lastMutationScan>>"""
    if source.count(original) != 1:
        raise SystemExit("Memory copy recovery mutation anchor drifted; review the model")
    # C2: admit delete after an incomplete scan. The exact scan invariant must
    # reject the old missing-root-as-empty behavior; a tool failure cannot pass.
    delete_guard = """CopyDelete(w) == /\\ mounted[w] /\\ ~crashed[w]
                 /\\ scanComplete[w]"""
    if source.count(delete_guard) != 1:
        raise SystemExit("Memory copy scan mutation anchor drifted; review the model")
    mutations = [
        (source.replace(original, mutant), "CopyRecoveryKeepsMaterializationBase"),
        (source.replace(delete_guard, "CopyDelete(w) == /\\ mounted[w] /\\ ~crashed[w]"),
         "CopyMutationRequiresCompleteScan"),
    ]
    for mutated, invariant in mutations:
        with tempfile.TemporaryDirectory(prefix="awaken-memory-copy-mutant-") as temp:
            directory = Path(temp)
            model = directory / "MemoryCAS.tla"
            model.write_text(mutated, encoding="utf-8")
            config = directory / "MemoryCASCopy.cfg"
            shutil.copy2(ROOT / "formal/tla/MemoryCASCopy.cfg", config)
            result = subprocess.run(
                ["java", "-Xmx512m", "-jar", str(tlc_jar.resolve()),
                 "-metadir", str(directory / "states"), "-config", str(config), str(model)],
                cwd=directory, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                timeout=30, check=False,
            )
            if result.returncode != 12 or f"Invariant {invariant} is violated" not in result.stdout:
                raise SystemExit(f"Memory copy mutant was not killed correctly:\n{result.stdout}")
    print("Memory copy mutation score: 2/2 killed (frozen base and complete scan)")


if __name__ == "__main__":
    main()
