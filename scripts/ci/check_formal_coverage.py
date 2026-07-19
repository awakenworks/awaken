#!/usr/bin/env python3
"""Validate the versioned formal-safety obligation coverage ledger."""

import json
import pathlib
import sys


ROOT = pathlib.Path(__file__).resolve().parents[2]
LEDGER = ROOT / "formal" / "coverage.json"
ALLOWED = {"machine_linked", "modeled_only", "executable_only", "external"}


def fail(message: str) -> None:
    print(f"formal coverage: {message}", file=sys.stderr)
    raise SystemExit(1)


data = json.loads(LEDGER.read_text(encoding="utf-8"))
minimum = float(data["minimum_ratio"])
obligations = data["obligations"]
if not 0.0 < minimum <= 1.0:
    fail(f"minimum_ratio must be in (0, 1], got {minimum}")
if not obligations:
    fail("the obligation ledger is empty")

ids: set[str] = set()
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
        relative.startswith("formal/tla/") and relative.endswith((".tla", ".cfg"))
        for relative in evidence
    )
    has_executable_link = any(
        relative.startswith("crates/") and relative.endswith(".rs")
        for relative in evidence
    )
    if status == "machine_linked":
        if not has_formal_model:
            fail(f"{obligation_id} is machine-linked without a formal model or proof")
        if not has_executable_link:
            fail(f"{obligation_id} is machine-linked without executable Rust evidence")
    elif status == "modeled_only" and not has_formal_model:
        fail(f"{obligation_id} is modeled-only without a formal model")
    elif status == "executable_only" and not has_executable_link:
        fail(f"{obligation_id} is executable-only without executable Rust evidence")

formalizable = [item for item in obligations if item["formalizable"]]
linked = [item for item in formalizable if item["status"] == "machine_linked"]
ratio = len(linked) / len(formalizable) if formalizable else 0.0
print(
    f"formal safety coverage: {len(linked)}/{len(formalizable)} "
    f"machine-linked obligations = {ratio:.1%} (required {minimum:.1%})"
)
if ratio < minimum:
    fail("coverage is below the required threshold")
