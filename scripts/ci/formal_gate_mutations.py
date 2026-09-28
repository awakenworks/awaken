"""Metadata mutants of the formal gates: the ledger, the anchors and the plan.

Each mutant edits a private copy of the repository's formal tree (and, when it
needs to, a private copy of one Rust source) and requires the authoritative
gate to reject it with the exact expected reason. Consumer: the no-argument run
of ``check_formal_mutations.py``, which reports the mutation score.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
import shutil

import formal_anchors
import formal_ledger


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


# Repository files a mutation made private to its case, with their bytes, so
# the loop can restore them with the formal/ tree.
PRIVATE_ORIGINALS: dict[Path, bytes] = {}


def private_source(case_root: Path, relative: str) -> Path:
    """Make one symlinked repository file writable inside the case only.

    A symlinked directory on the way is replaced by a mirror of its subtree --
    real directories, symlinked files -- because a recursive glob does not
    descend through a directory link; the file itself becomes a copy, so a
    mutation can never write through a link into the repository it was
    copied from.
    """

    current = case_root
    for part in Path(relative).parts[:-1]:
        current = current / part
        if current.is_symlink():
            target = current.resolve()
            current.unlink()
            for directory, _, names in os.walk(target):
                mirror = current / Path(directory).relative_to(target)
                mirror.mkdir(parents=True, exist_ok=True)
                for name in names:
                    os.symlink(Path(directory) / name, mirror / name)
    path = case_root / relative
    if path.is_symlink():
        content = path.read_bytes()
        path.unlink()
        path.write_bytes(content)
        PRIVATE_ORIGINALS[path] = content
    if case_root.resolve() not in path.resolve().parents:
        raise SystemExit(f"mutation fixture could not privatize {relative}")
    return path


def remove_lines(case_root: Path, anchors: list[formal_anchors.Anchor]) -> None:
    for relative in sorted({anchor.path for anchor in anchors}):
        path = private_source(case_root, relative)
        drop = {anchor.line for anchor in anchors if anchor.path == relative}
        lines = path.read_text().split("\n")
        path.write_text("\n".join(line for number, line in enumerate(lines, 1) if number not in drop))


def mutation_targets(case_root: Path) -> dict[Path, bytes]:
    paths = [path for path in (case_root / "formal").rglob("*") if path.is_file()]
    return {path: path.read_bytes() for path in [*paths, case_root / "scripts/ci/check_formal.sh"]}


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def mutate_removed_harness(case_root: Path) -> None:
    # H0: a harness loses its #[kani::proof] attribute while its anchors stay.
    # Nothing can quietly drop out of strict CI: the anchors now dangle.
    harness = next(item for item in formal_anchors.scan(case_root).harnesses if item.values("obligation"))
    path = private_source(case_root, harness.path)
    lines = path.read_text().split("\n")
    attribute = max(index for index in range(harness.line - 1) if lines[index].strip() == "#[kani::proof]")
    del lines[attribute]
    path.write_text("\n".join(lines))
    anchor = harness.values("obligation")[0]
    must_fail(case_root, "check_formal_coverage.py", "removed Kani harness from strict CI",
              expected=f"{anchor.where}: @obligation is not in the attribute block of a #[kani::proof] fn")
def mutate_downgraded_claim(case_root: Path) -> None:
    # D1: a proved row loses every `/// @obligation` anchor (each harness still
    # serves another row, so only this claim changes) and is relabeled
    # executable-only while keeping its production-kernel grade.
    harnesses = formal_anchors.scan(case_root).harnesses
    ledger = formal_ledger.load(case_root)
    row = next(
        item for item in ledger["obligations"]
        if item["status"] == "kernel_proved" and item.get("proof_harnesses") and all(
            len(harness.values("obligation")) > 1
            for harness in harnesses if harness.name in item["proof_harnesses"]
        )
    )
    remove_lines(case_root, [
        anchor for harness in harnesses for anchor in harness.values("obligation")
        if anchor.value == row["id"]
    ])
    row["status"] = "executable_only"
    row.pop("proof_harnesses")
    formal_ledger.write(case_root, ledger)
    must_fail(case_root, "check_formal_coverage.py", "downgraded proved obligation",
              expected=f"{row['id']} production-kernel grade has no Kani harness")


def mutate_unanchored_harness(case_root: Path) -> None:
    # D2: a harness loses its only anchor. A proof that serves no obligation is
    # refused at its own source line, not silently dropped from the ledger.
    harness = next(item for item in formal_anchors.scan(case_root).harnesses
                   if len(item.values("obligation")) == 1)
    remove_lines(case_root, harness.values("obligation"))
    must_fail(case_root, "check_formal_coverage.py", "harness without an obligation anchor",
              expected=f"Kani harness {harness.name} serves no obligation")


def mutate_orphaned_anchor(case_root: Path) -> None:
    # D3: an obligation file is deleted while a harness still names it. The
    # commit that deletes it fails, at the anchor's file and line.
    harness = next(item for item in formal_anchors.scan(case_root).harnesses if item.values("obligation"))
    anchor = harness.values("obligation")[0]
    prefix, _, name = anchor.value.partition(".")
    (case_root / formal_ledger.OBLIGATION_DIRECTORY / prefix / f"{name}.toml").unlink()
    must_fail(case_root, "check_formal_coverage.py", "obligation file deleted under a live anchor",
              expected=f"{anchor.where}: @obligation {anchor.value!r} names no obligation file")


def mutate_dropped_entrypoint_anchor(case_root: Path) -> None:
    # E1: a direct production proof loses the only `// @formal entrypoint`
    # line naming its production code. The claim loses its entrypoint with it.
    headers, _ = formal_anchors.headers(case_root)
    entrypoints: dict[str, list[formal_anchors.Header]] = {}
    for header in headers:
        if header.role == "entrypoint":
            entrypoints.setdefault(header.value, []).append(header)
    row = next(item for item in formal_ledger.load(case_root)["obligations"]
               if item["proof_grade"] == "production_kernel_proved"
               and len(entrypoints.get(item["id"], [])) == len(item["production_entrypoints"]) == 1)
    [header] = entrypoints[row["id"]]
    path = private_source(case_root, header.path)
    lines = path.read_text().split("\n")
    del lines[header.line - 1]
    path.write_text("\n".join(lines))
    must_fail(case_root, "check_formal_coverage.py", "production entrypoint anchor deleted",
              expected=f"{row['id']} direct production grade has no production entrypoint")


def mutate_source_evidence_in_storage(case_root: Path) -> None:
    # E2: an obligation file names a Rust source again. The ratchet refuses it,
    # so a new or edited obligation must be anchored in its sources.
    header = next(item for item in formal_anchors.headers(case_root)[0] if item.role == "evidence")
    prefix, _, name = header.value.partition(".")
    path = case_root / formal_ledger.OBLIGATION_DIRECTORY / prefix / f"{name}.toml"
    text = path.read_text()
    if "evidence = []\n" in text:
        text = text.replace("evidence = []\n", f'evidence = ["{header.path}"]\n', 1)
    else:
        text = text.replace("evidence = [\n", f'evidence = [\n  "{header.path}",\n', 1)
    path.write_text(text)
    must_fail(case_root, "check_formal_coverage.py", "Rust evidence stored in an obligation file",
              expected=f"names {header.path}, a source that states its own role")


def mutate_commented_harness(case_root: Path) -> None:
    # H1: the plan is still computed but the loop that runs its batches becomes
    # a shell comment. Text presence is not execution selection; reject it.
    gate = case_root / "scripts/ci/check_formal.sh"
    source = gate.read_text()
    needle = '    run_kani "${kani_argv[@]}"'
    if source.count(needle) != 1:
        raise SystemExit("mutation fixture lost the derived Kani plan loop")
    gate.write_text(source.replace(needle, "    # " + needle.lstrip(), 1))
    must_fail(case_root, "check_formal_coverage.py", "comment-only Kani selection",
              expected="strict CI does not run the derived Kani plan exactly once")
def mutate_wrong_harness_package(case_root: Path) -> None:
    # H2: a harness borrows a profile whose loop identities are mangled against
    # another crate. Package ownership comes from Cargo, and a package-bound
    # profile cannot be carried to a different package's proof.
    harness = next(item for item in formal_anchors.scan(case_root).harnesses
                   if item.package == "awaken-work-store" and not item.values("kani"))
    path = private_source(case_root, harness.path)
    lines = path.read_text().split("\n")
    attribute = max(index for index in range(harness.line - 1) if lines[index].strip() == "#[kani::proof]")
    indent = lines[attribute][: len(lines[attribute]) - len(lines[attribute].lstrip())]
    lines.insert(attribute, f"{indent}/// @kani profile=fingerprint-fnv-hex-v1")
    path.write_text("\n".join(lines))
    must_fail(case_root, "check_formal_coverage.py", "Kani selection in wrong package",
              expected="Kani profile fingerprint-fnv-hex-v1 is bound to package awaken-agent-contract, not awaken-work-store")
def mutate_harness_name_prefix(case_root: Path) -> None:
    # H3: adding a short name that is merely a prefix of a real proof, plus a
    # comment mentioning that name, cannot manufacture a proof definition.
    # The anchor sits on an ordinary fn whose only proof attribute is inside a
    # comment: it is dangling, so it cannot name a harness into the ledger.
    identifier = "work.persisted_epoch_is_exact_nonnegative_and_nonwrapping"
    harness = next(item for item in formal_anchors.scan(case_root).harnesses
                   if any(anchor.value == identifier for anchor in item.values("obligation")))
    path = private_source(case_root, harness.path)
    path.write_text(path.read_text() + f"\n// #[kani::proof]\n/// @obligation {identifier}\nfn work_lease_epoch() {{}}\n")
    must_fail(case_root, "check_formal_coverage.py", "prefix is not a proof definition",
              expected="@obligation is not in the attribute block of a #[kani::proof] fn (dangling anchor)")


def mutate_duplicate_harness_selection(case_root: Path) -> None:
    # H4: running the plan twice cannot satisfy a second proof identity or
    # obscure an ambiguous runner binding. One plan, one loop, one run.
    gate = case_root / "scripts/ci/check_formal.sh"
    source = gate.read_text()
    needle = '  kani_plan="$(python3 scripts/ci/formal_plan.py kani --tier full)"\n'
    if source.count(needle) != 1:
        raise SystemExit("mutation fixture lost the derived Kani plan assignment")
    gate.write_text(source.replace(needle, needle * 2, 1))
    must_fail(case_root, "check_formal_coverage.py", "duplicate Kani selection",
              expected="strict CI does not run the derived Kani plan exactly once")
def mutate_unchecked_model(case_root: Path) -> None:
    gate = case_root / "scripts" / "ci" / "check_formal.sh"
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
    gate = case_root / "scripts/ci/check_formal.sh"
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


def mutate_missing_proof_grade(case_root: Path) -> None:
    ledger = formal_ledger.load(case_root)
    ledger["obligations"][0].pop("proof_grade")
    formal_ledger.write(case_root, ledger)
    must_fail(case_root, "check_formal_coverage.py", "proof grade removed",
              expected=f"{ledger['obligations'][0]['id']} has unknown or missing proof_grade None")


def mutate_false_production_grade(case_root: Path) -> None:
    ledger = formal_ledger.load(case_root)
    row = next(
        item
        for item in ledger["obligations"]
        if item["proof_grade"] == "abstract_model_checked"
        and not item.get("proof_harnesses")
    )
    row["proof_grade"] = "production_kernel_proved"
    formal_ledger.write(case_root, ledger)
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
    ledger = formal_ledger.load(case_root)
    row = next(item for item in ledger["obligations"] if
               "crates/runtime/awaken-runtime/tests/formal_refinement.rs" in item["evidence"])
    row["proof_grade"] = "implementation_refined"
    formal_ledger.write(case_root, ledger)
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



def run_gate_mutations() -> None:
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
        mutate_unanchored_harness,
        mutate_orphaned_anchor,
        mutate_dropped_entrypoint_anchor,
        mutate_source_evidence_in_storage,
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
            # Only the copied formal/ tree and the copied gate are writable
            # mutation targets. Restore exact bytes (and drop any file a
            # mutation added) and prove the baseline again so a secondary
            # fixture failure cannot count as the requested kill.
            original = mutation_targets(case_root)
            mutation(case_root)
            for path in set(mutation_targets(case_root)) - set(original):
                path.unlink()
            for path, content in [*original.items(), *PRIVATE_ORIGINALS.items()]:
                path.write_bytes(content)
            PRIVATE_ORIGINALS.clear()
            require_green_baseline(case_root)
    print(f"formal gate mutation score: {len(mutations)}/{len(mutations)} killed")
