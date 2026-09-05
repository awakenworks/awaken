#!/usr/bin/env python3
"""Validate the versioned formal-safety obligation coverage ledger."""

import argparse
import json
import pathlib
import re
import sys

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
    has_executable_link = any(
        relative.startswith("crates/") and relative.endswith(".rs")
        for relative in evidence
    )
    for relative in evidence:
        if relative.startswith("formal/tla/") and relative not in formal_gate:
            fail(f"{obligation_id} formal evidence {relative} is absent from strict CI")

    harnesses = obligation.get("proof_harnesses", [])
    if len(harnesses) != len(set(harnesses)):
        fail(f"{obligation_id} contains duplicate proof harnesses")
    if harnesses:
        ledger_harnesses.update(harnesses)
        source = "\n".join(
            (ROOT / relative).read_text(encoding="utf-8")
            for relative in evidence
            if relative.startswith("crates/") and relative.endswith(".rs")
        )
        for harness in harnesses:
            if f"fn {harness}" not in source:
                fail(f"{obligation_id} names missing Kani harness {harness}")
            if f"--harness {harness}" not in formal_gate:
                fail(f"{obligation_id} Kani harness {harness} is absent from strict CI")

    if status == "model_linked":
        if not has_formal_model:
            fail(f"{obligation_id} is model-linked without a formal model or proof")
        if not has_executable_link:
            fail(f"{obligation_id} is model-linked without executable Rust evidence")
    elif status == "kernel_proved":
        if not has_executable_link:
            fail(f"{obligation_id} is kernel-proved without production Rust evidence")
        if not harnesses:
            fail(f"{obligation_id} is kernel-proved without named Kani harnesses")
    elif status == "modeled_only" and not has_formal_model:
        fail(f"{obligation_id} is modeled-only without a formal model")
    elif status == "executable_only" and not has_executable_link:
        fail(f"{obligation_id} is executable-only without executable Rust evidence")

    if proof_grade == "abstract_model_checked" and not has_formal_model:
        fail(f"{obligation_id} model-checked grade has no checked model")
    if proof_grade == "abstract_theorem_proved" and not any(
        relative.endswith("Proof.tla") for relative in evidence
    ):
        fail(f"{obligation_id} theorem-proved grade has no TLAPS proof")
    for relative in production_entrypoints:
        if relative not in evidence:
            fail(f"{obligation_id} production entrypoint is not evidence: {relative}")
        if not relative.startswith("crates/") or not relative.endswith(".rs"):
            fail(f"{obligation_id} production entrypoint is not Rust source: {relative}")
        if (
            "/tests/" in relative
            or relative.endswith("/tests.rs")
            or "-testkit/" in relative
        ):
            fail(f"{obligation_id} test support is not a production entrypoint: {relative}")

source_harnesses: dict[str, list[str]] = {}
proof_pattern = re.compile(
    r"#\[kani::proof\]\s*(?:#\[[^\]]+\]\s*)*fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)
for path in (ROOT / "crates").glob("**/*.rs"):
    relative = path.relative_to(ROOT).as_posix()
    for harness in proof_pattern.findall(path.read_text(encoding="utf-8")):
        source_harnesses.setdefault(harness, []).append(relative)

for harness, paths in sorted(source_harnesses.items()):
    if f"--harness {harness}" not in formal_gate:
        fail(
            f"source Kani harness {harness} in {', '.join(paths)} is absent from strict CI"
        )
    if harness not in ledger_harnesses:
        fail(
            f"source Kani harness {harness} in {', '.join(paths)} is absent from the obligation ledger"
        )

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
