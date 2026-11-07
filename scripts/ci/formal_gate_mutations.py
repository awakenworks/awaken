"""Strict-CI wiring mutants of the formal gate.

check_formal_coverage reads scripts/ci/check_formal.sh to decide that the
derived Kani plan is computed exactly once, that every cited TLA+ model is checked and
that each release entrypoint's self-test runs. No fixture self-test reaches
those rules, so each mutant edits a private copy of the gate and requires the
checker to reject it with the exact reason. The ledger, anchor, plan, profile,
feature and boundary rejections are held by their own self-tests
(formal_ledger, formal_plan, test_formal_profile); that every planned batch
reaches the runner is test_formal_profile row P4, which executes the gate's loop. Consumer:
``check_formal_mutations.py --gate-mutants`` in the full tier.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
from pathlib import Path
import shutil

import formal_ledger


ROOT = Path(__file__).resolve().parents[2]
GATE = "scripts/ci/check_formal.sh"


def prepare_case(case_root: Path) -> None:
    # Copy every writable mutation target and the checker's dependencies; link
    # the rest. An unmutated case must be green, or any nonzero exit would
    # report a false kill (require_green_baseline).
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
            cwd=case_root, text=True, capture_output=True, check=False, timeout=600,
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
    result = run_checker(case_root, "check_formal_coverage.py", "--require-evidence-complete")
    if result.returncode != 0:
        raise SystemExit(f"formal mutation baseline failed: check_formal_coverage.py: {result.stderr}")


def mutate_duplicate_harness_selection(case_root: Path) -> None:
    # H4: running the plan twice cannot satisfy a second proof identity or
    # obscure an ambiguous runner binding. One plan, one loop, one run.
    gate = case_root / GATE
    source = gate.read_text()
    needle = '  kani_plan="$(python3 scripts/ci/formal_plan.py kani "${formal_selection[@]}")"\n'
    if source.count(needle) != 1:
        raise SystemExit("mutation fixture lost the derived Kani plan assignment")
    gate.write_text(source.replace(needle, needle * 2, 1))
    must_fail(case_root, "check_formal_coverage.py", "duplicate Kani selection",
              expected="strict CI does not run the derived Kani plan exactly once")


def mutate_unchecked_model(case_root: Path) -> None:
    # M1: a TLA+ model an obligation cites drops out of the gate text.
    gate = case_root / GATE
    source = gate.read_text()
    ledger = formal_ledger.load(case_root)
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
    # P1: the Python authority, model and direct Rust kernel still exist, but the
    # production entrypoint's only strict-CI self-test is commented out. Static
    # source/harness presence cannot replace execution of the composing owner.
    gate = case_root / GATE
    source = gate.read_text()
    anchor = "python3 scripts/release/verification_manifest.py self-test"
    if source.count(anchor) != 1:
        raise SystemExit("release self-test mutation anchor drifted")
    gate.write_text(source.replace(anchor, "# " + anchor))
    # The checker reports the first owner in ledger order; name that exact row.
    owner = next(row["id"] for row in formal_ledger.load(case_root)["obligations"]
                 if "scripts/release/verification_manifest.py" in row["production_entrypoints"])
    must_fail(case_root, "check_formal_coverage.py", "comment-only Python self-test",
              expected=f"{owner} release entrypoint needs exactly one strict-CI self-test: scripts/release/verification_manifest.py")


def run_gate_mutations() -> None:
    # Each mutant must make check_formal_coverage fail with its exact reason;
    # no mutant may introduce a second ledger or a test-only acceptance path.
    mutations = (
        mutate_commented_release_self_test,
        mutate_duplicate_harness_selection,
        mutate_unchecked_model,
    )
    with tempfile.TemporaryDirectory(prefix="awaken-formal-mutations-") as temp:
        case_root = Path(temp)
        prepare_case(case_root)
        gate = case_root / GATE
        original = gate.read_bytes()
        for mutation in mutations:
            # Only the copied gate is a mutation target. Restore its exact
            # bytes and prove the baseline again so a secondary fixture failure
            # cannot count as the requested kill.
            require_green_baseline(case_root)
            mutation(case_root)
            gate.write_bytes(original)
        require_green_baseline(case_root)
    print(f"formal gate mutation score: {len(mutations)}/{len(mutations)} killed")
