#!/usr/bin/env python3
"""Require an architecture plan for every Rust or Web product-only surface."""

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
KINDS = {"external", "unbounded_input", "effect_adapter", "semantic", "ui_or_human"}
FIELDS = {
    "requirement_id", "boundary_kind", "why_not_directly_proved",
    "architecture_change", "target_formal_method", "acceptance_gate",
}


def read(path: str):
    return json.loads((ROOT / path).read_text())


def declared_residual_requirements() -> set[str]:
    result = set()
    for feature in read("formal/features.json")["features"]:
        for requirement in feature["requirements"]:
            if requirement.get("residual_boundary", False):
                result.add(f'{feature["id"]}.{requirement["id"]}')
    return result


def main() -> None:
    surfaces = read("formal/surface-classifications.json")["classifications"]
    web_surfaces = read("formal/web-surface-classifications.json")["classifications"]
    expected = {row["requirement_id"] for row in surfaces if "requirement_id" in row}
    expected.update(row["requirement_id"] for row in web_surfaces)
    # A production source may consume a proved kernel while its transport or
    # environmental tail remains outside the repository theorem. Such a
    # boundary no longer has a requirement-only source row, so it is declared
    # explicitly in the canonical feature inventory instead of being dropped.
    expected.update(declared_residual_requirements())
    rows = read("formal/proof-boundaries.json")["boundaries"]
    actual = set()
    errors = []
    for index, row in enumerate(rows):
        missing = FIELDS - row.keys()
        if missing:
            errors.append(f"row {index} missing {sorted(missing)}")
            continue
        rid = row["requirement_id"]
        if rid in actual:
            errors.append(f"duplicate {rid}")
        actual.add(rid)
        if row["boundary_kind"] not in KINDS:
            errors.append(f"{rid}: invalid boundary_kind")
        for field in FIELDS - {"requirement_id", "boundary_kind"}:
            if not isinstance(row[field], str) or not row[field].strip():
                errors.append(f"{rid}: empty {field}")
    if expected - actual:
        errors.append(f"missing {sorted(expected - actual)}")
    if actual - expected:
        errors.append(f"extra {sorted(actual - expected)}")
    if errors:
        raise SystemExit("proof boundaries: " + "; ".join(errors))
    print(f"proof boundaries: {len(actual)}/{len(expected)} residual requirements documented, 0 missing, 0 extra")


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for: that is how
# `check-all.sh` invoked `--selftest` for years while the flag was `--self-test`
# and the self-test never once executed. ``allow_abbrev=False`` closes the
# second half: without it ``--self-tes`` is also accepted, so the flag has no
# single spelling.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_proof_boundaries.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument("paths", nargs="*", help="ignored; the residual-boundary ledger is read whole")
    return parser.parse_args(argv)


if __name__ == "__main__":
    parse(sys.argv[1:])
    main()
