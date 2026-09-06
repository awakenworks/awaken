#!/usr/bin/env python3
"""Validate the versioned formal-safety obligation coverage ledger."""

import argparse
import json
import pathlib
import re
import shlex
import sys
import tomllib

from _crate_boundary_workspace import rust_syntax
from formal_profile import PROOF_GRADES, profile_result, validate_proof_grade


ROOT = pathlib.Path(__file__).resolve().parents[2]
LEDGER = ROOT / "formal" / "coverage.json"
ALLOWED = {
    "model_linked",
    "kernel_proved",
    "modeled_only",
    "executable_only",
    "external",
}
CLAIM_SCOPES = {"repository_safety", "external_assumption", "semantic_nonclaim"}


def fail(message: str) -> None:
    print(f"formal coverage: {message}", file=sys.stderr)
    raise SystemExit(1)


parser = argparse.ArgumentParser()
parser.add_argument("--json", action="store_true")
parser.add_argument(
    "--require-complete",
    action="store_true",
    help="deprecated alias for --require-evidence-complete",
)
parser.add_argument(
    "--require-evidence-complete",
    action="store_true",
    help="require every formalizable obligation to have checked formal evidence",
)
parser.add_argument(
    "--require-verified-profile",
    action="store_true",
    help="require direct production proof/refinement for every P0 repository safety obligation",
)
args = parser.parse_args()

data = json.loads(LEDGER.read_text(encoding="utf-8"))
minimum = float(data["minimum_ratio"])
obligations = data["obligations"]
profile = data.get("verified_profile")
formal_gate = (ROOT / "scripts/ci/check_formal.sh").read_text(encoding="utf-8")
# Inventory the existing runner's literal selections, not arbitrary shell text.
# This checks its declared package/name bindings; execution is still witnessed
# by the strict runner receipt, not by this static parser.
gate_harnesses: dict[str, list[str]] = {}
gate_python_self_tests: dict[str, int] = {}
for line in formal_gate.replace("\\\n", " ").splitlines():
    if line.lstrip().startswith("python3 "):
        selection = shlex.split(line, comments=True)
        if len(selection) == 3 and selection[2] == "self-test":
            relative = selection[1]
            gate_python_self_tests[relative] = gate_python_self_tests.get(relative, 0) + 1
    if not line.lstrip().startswith("run_kani "):
        continue
    tokens = shlex.split(line, comments=True)
    if len(tokens) < 4 or re.fullmatch(r"[a-z0-9_-]+", tokens[1]) is None:
        fail("strict CI Kani selection must name a literal package and exact harnesses")
    index = 2
    while index < len(tokens):
        flag = tokens[index]
        if flag == "--no-default-features":
            index += 1
            continue
        if index + 1 >= len(tokens):
            fail("strict CI Kani selection must name a literal package and exact harnesses")
        value = tokens[index + 1]
        if flag == "--harness" and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", value):
            gate_harnesses.setdefault(value, []).append(tokens[1])
        elif flag not in {"--features", "--solver"} or re.fullmatch(r"[A-Za-z0-9_,.-]+", value) is None:
            fail("strict CI Kani selection must name a literal package and exact harnesses")
        index += 2

source_harnesses: dict[str, list[str]] = {}
source_packages: dict[str, str] = {}
proof_pattern = re.compile(
    r"#\[kani::proof\]\s*(?:#\[[^\]]+\]\s*)*fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)
for path in (ROOT / "crates").glob("**/*.rs"):
    source = path.read_text(encoding="utf-8")
    if "#[kani::proof]" not in source:
        continue
    relative = path.relative_to(ROOT).as_posix()
    # Reuse the existing Rust lexer: comments, doc examples and string literals
    # cannot create a definition, and a function-name prefix is not an identity.
    harnesses = proof_pattern.findall(rust_syntax(source))
    if not harnesses:
        continue
    manifest = next((parent / "Cargo.toml" for parent in path.parents
                     if parent != ROOT and (parent / "Cargo.toml").is_file()), None)
    if manifest is None:
        fail(f"Kani source has no Cargo package: {relative}")
    source_packages[relative] = tomllib.loads(manifest.read_text())["package"]["name"]
    for harness in harnesses:
        source_harnesses.setdefault(harness, []).append(relative)
if not 0.0 < minimum <= 1.0:
    fail(f"minimum_ratio must be in (0, 1], got {minimum}")
if not obligations:
    fail("the obligation ledger is empty")
if not isinstance(profile, dict) or profile.get("schema_version") != 1:
    fail("verified_profile schema_version must be 1")
if profile.get("id") != "awaken-verified-v1":
    fail("verified_profile id must be awaken-verified-v1")

features = json.loads((ROOT / "formal" / "features.json").read_text(encoding="utf-8"))
assumption_data = json.loads(
    (ROOT / "formal" / "assumptions.json").read_text(encoding="utf-8")
)
assumption_ids = {item["id"] for item in assumption_data["assumptions"]}

ids: set[str] = set()
ledger_harnesses: set[str] = set()
for obligation in obligations:
    obligation_id = obligation["id"]
    if obligation_id in ids:
        fail(f"duplicate obligation id {obligation_id}")
    ids.add(obligation_id)
    status = obligation["status"]
    if status not in ALLOWED:
        fail(f"{obligation_id} has unknown status {status}")
    if obligation["formalizable"] and status == "external":
        fail(f"{obligation_id} is formalizable but marked external")
    if not obligation["formalizable"] and status != "external":
        fail(f"{obligation_id} is external but counted in formal coverage")
    claim_scope = obligation.get("claim_scope")
    proof_grade = obligation.get("proof_grade")
    bounds = obligation.get("bounds")
    tcb_assumptions = obligation.get("tcb_assumptions")
    production_entrypoints = obligation.get("production_entrypoints")
    if claim_scope not in CLAIM_SCOPES:
        fail(f"{obligation_id} has unknown or missing claim_scope {claim_scope!r}")
    try:
        validate_proof_grade(obligation)
    except ValueError as error:
        fail(str(error))
    if not isinstance(bounds, list) or not bounds or not all(isinstance(v, str) and v for v in bounds):
        fail(f"{obligation_id} bounds must be a non-empty string list")
    if not isinstance(tcb_assumptions, list) or not all(isinstance(v, str) and v for v in tcb_assumptions):
        fail(f"{obligation_id} tcb_assumptions must be a string list")
    unknown_tcb = sorted(set(tcb_assumptions) - assumption_ids)
    if unknown_tcb:
        fail(f"{obligation_id} names unknown TCB assumptions: {', '.join(unknown_tcb)}")
    if not isinstance(production_entrypoints, list) or not all(isinstance(v, str) and v for v in production_entrypoints):
        fail(f"{obligation_id} production_entrypoints must be a string list")
    if obligation["formalizable"] and claim_scope != "repository_safety":
        fail(f"{obligation_id} formalizable claim must use repository_safety scope")
    if not obligation["formalizable"] and proof_grade not in {"external_tcb", "semantic_nonclaim"}:
        fail(f"{obligation_id} external claim has repository proof grade {proof_grade}")
    if proof_grade == "semantic_nonclaim" and claim_scope != "semantic_nonclaim":
        fail(f"{obligation_id} semantic nonclaim must use semantic_nonclaim scope")
    evidence = obligation.get("evidence", [])
    if not evidence:
        fail(f"{obligation_id} has no evidence")
    if len(evidence) != len(set(evidence)):
        fail(f"{obligation_id} contains duplicate evidence paths")
    for relative in evidence:
        path = ROOT / relative
        if not path.is_file():
            fail(f"{obligation_id} references missing evidence {relative}")

    has_formal_model = any(
        relative.startswith("formal/tla/") and relative.endswith(".tla")
        for relative in evidence
    )
    has_rust_link = any(
        relative.startswith("crates/") and relative.endswith(".rs")
        for relative in evidence
    )
    # Model linkage may describe a real release-side Python authority. It must
    # be an explicit production entrypoint with one literal strict-CI self-test
    # selection; arbitrary scripts/comments are not executable linkage. This
    # adds no direct-proof credit and does not relax Rust/Kani kernel evidence.
    release_python = {
        relative for relative in production_entrypoints
        if re.fullmatch(r"scripts/release/[A-Za-z_][A-Za-z0-9_]*\.py", relative)
        and not pathlib.PurePosixPath(relative).name.startswith("test_")
    }
    has_executable_link = has_rust_link or any(
        relative in evidence and gate_python_self_tests.get(relative) == 1
        for relative in release_python
    )
    for relative in evidence:
        if relative.startswith("formal/tla/") and relative not in formal_gate:
            fail(f"{obligation_id} formal evidence {relative} is absent from strict CI")

    harnesses = obligation.get("proof_harnesses", [])
    if len(harnesses) != len(set(harnesses)):
        fail(f"{obligation_id} contains duplicate proof harnesses")
    if harnesses:
        ledger_harnesses.update(harnesses)
        for harness in harnesses:
            definitions = source_harnesses.get(harness, [])
            if len(definitions) != 1 or definitions[0] not in evidence:
                fail(f"{obligation_id} Kani harness {harness} is not a unique proof definition in its evidence")
            selected = gate_harnesses.get(harness, [])
            if not selected:
                fail(f"{obligation_id} Kani harness {harness} is absent from strict CI")
            if selected != [source_packages[definitions[0]]]:
                fail(f"{obligation_id} Kani harness {harness} is selected from wrong package or more than once")

    if status == "model_linked":
        if not has_formal_model:
            fail(f"{obligation_id} is model-linked without a formal model or proof")
        if not has_executable_link:
            fail(f"{obligation_id} is model-linked without executable production evidence")
    elif status == "kernel_proved":
        if not has_rust_link:
            fail(f"{obligation_id} is kernel-proved without production Rust evidence")
        if not harnesses:
            fail(f"{obligation_id} is kernel-proved without named Kani harnesses")
    elif status == "modeled_only" and not has_formal_model:
        fail(f"{obligation_id} is modeled-only without a formal model")
    elif status == "executable_only" and not has_executable_link:
        fail(f"{obligation_id} is executable-only without executable production evidence")

    if proof_grade == "abstract_model_checked" and not has_formal_model:
        fail(f"{obligation_id} model-checked grade has no checked model")
    if proof_grade == "abstract_theorem_proved" and not any(
        relative.endswith("Proof.tla") for relative in evidence
    ):
        fail(f"{obligation_id} theorem-proved grade has no TLAPS proof")
    for relative in production_entrypoints:
        if relative not in evidence:
            fail(f"{obligation_id} production entrypoint is not evidence: {relative}")
        if relative in release_python:
            if gate_python_self_tests.get(relative) != 1:
                fail(f"{obligation_id} release entrypoint needs exactly one strict-CI self-test: {relative}")
        elif not relative.startswith("crates/") or not relative.endswith(".rs"):
            fail(f"{obligation_id} production entrypoint is not Rust or checked release Python source: {relative}")
        if (
            "/tests/" in relative
            or relative.endswith("/tests.rs")
            or "-testkit/" in relative
        ):
            fail(f"{obligation_id} test support is not a production entrypoint: {relative}")

for harness, paths in sorted(source_harnesses.items()):
    if harness not in gate_harnesses:
        fail(
            f"source Kani harness {harness} in {', '.join(paths)} is absent from strict CI"
        )
    if harness not in ledger_harnesses:
        fail(
            f"source Kani harness {harness} in {', '.join(paths)} is absent from the obligation ledger"
        )
for harness in sorted(set(gate_harnesses) - set(source_harnesses)):
    fail(f"strict CI selects nonexistent Kani harness {harness}")

formalizable = [item for item in obligations if item["formalizable"]]
linked = [
    item
    for item in formalizable
    if item["status"] in {"model_linked", "kernel_proved"}
]
ratio = len(linked) / len(formalizable) if formalizable else 0.0
model_checked = [
    item
    for item in formalizable
    if any(
        relative.startswith("formal/tla/") and relative.endswith(".tla")
        for relative in item.get("evidence", [])
    )
]
model_proved = [
    item
    for item in formalizable
    if any(
        relative.startswith("formal/tla/") and relative.endswith("Proof.tla")
        for relative in item.get("evidence", [])
    )
]
kernel_proved = [
    item
    for item in formalizable
    if item["status"] == "kernel_proved" or item.get("proof_harnesses")
]
trace_refined = [
    item
    for item in formalizable
    if "crates/runtime/awaken-runtime/tests/formal_refinement.rs"
    in item.get("evidence", [])
]
executable_only = [
    item for item in formalizable if item["status"] == "executable_only"
]
summary = {
    "formalizable": len(formalizable),
    "checked_formal_evidence": len(linked),
    "checked_formal_evidence_ratio": ratio,
    "model_checked": len(model_checked),
    "model_proved": len(model_proved),
    "kernel_proved": len(kernel_proved),
    "trace_refined": len(trace_refined),
    "executable_only": len(executable_only),
    "external": len(obligations) - len(formalizable),
    "proof_grades": {
        grade: sum(item["proof_grade"] == grade for item in obligations)
        for grade in sorted(PROOF_GRADES)
    },
}
try:
    qualification = profile_result(features, data)
except ValueError as error:
    fail(str(error))
verified_gaps = qualification["gaps"]
summary["verified_profile"] = {
    "id": profile["id"],
    "required": qualification["required_obligations"],
    "directly_proved_or_refined": qualification["directly_proved_or_refined"],
    "eligible": qualification["eligible"],
    "gaps": verified_gaps,
}
if args.json:
    print(json.dumps(summary, indent=2, sort_keys=True))
else:
    print(
        f"formal evidence coverage: {len(linked)}/{len(formalizable)} "
        f"model-linked or kernel-proved obligations = {ratio:.1%} "
        f"(required {minimum:.1%}); "
        f"dimensions: model-checked={len(model_checked)}, "
        f"model-proved={len(model_proved)}, kernel-proved={len(kernel_proved)}, "
        f"trace-refined={len(trace_refined)}, executable-only={len(executable_only)}"
    )
if ratio < minimum:
    fail("coverage is below the required threshold")
if (args.require_complete or args.require_evidence_complete) and (ratio != 1.0 or executable_only):
    fail(
        "complete coverage required: every formalizable obligation must be "
        "model-linked or kernel-proved and none may remain executable-only"
    )
if args.require_verified_profile and verified_gaps:
    fail(
        f"verified profile blocked: {len(verified_gaps)}/{qualification['required_obligations']} "
        "P0 repository safety obligations lack direct production proof/refinement: "
        + ", ".join(verified_gaps)
    )
