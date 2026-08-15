#!/usr/bin/env python3
"""Validate the versioned formal-safety obligation coverage ledger."""

import json
import pathlib
import re
import sys


ROOT = pathlib.Path(__file__).resolve().parents[2]
LEDGER = ROOT / "formal" / "coverage.json"
ALLOWED = {
    "machine_linked",
    "kernel_proved",
    "modeled_only",
    "executable_only",
    "external",
}


def fail(message: str) -> None:
    print(f"formal coverage: {message}", file=sys.stderr)
    raise SystemExit(1)


data = json.loads(LEDGER.read_text(encoding="utf-8"))
minimum = float(data["minimum_ratio"])
obligations = data["obligations"]
formal_gate = (ROOT / "scripts/ci/check_formal.sh").read_text(encoding="utf-8")
if not 0.0 < minimum <= 1.0:
    fail(f"minimum_ratio must be in (0, 1], got {minimum}")
if not obligations:
    fail("the obligation ledger is empty")

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

    if status == "machine_linked":
        if not has_formal_model:
            fail(f"{obligation_id} is machine-linked without a formal model or proof")
        if not has_executable_link:
            fail(f"{obligation_id} is machine-linked without executable Rust evidence")
    elif status == "kernel_proved":
        if not has_executable_link:
            fail(f"{obligation_id} is kernel-proved without production Rust evidence")
        if not harnesses:
            fail(f"{obligation_id} is kernel-proved without named Kani harnesses")
    elif status == "modeled_only" and not has_formal_model:
        fail(f"{obligation_id} is modeled-only without a formal model")
    elif status == "executable_only" and not has_executable_link:
        fail(f"{obligation_id} is executable-only without executable Rust evidence")

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
    if item["status"] in {"machine_linked", "kernel_proved"}
]
ratio = len(linked) / len(formalizable) if formalizable else 0.0
print(
    f"formal safety coverage: {len(linked)}/{len(formalizable)} "
    f"machine-linked or kernel-proved obligations = {ratio:.1%} "
    f"(required {minimum:.1%})"
)
if ratio < minimum:
    fail("coverage is below the required threshold")
