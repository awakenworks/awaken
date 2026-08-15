#!/usr/bin/env python3
"""Inventory production Rust modules that carry formal-safety risk signals.

The obligation ledger is claim-oriented.  This complementary inventory is
source-oriented: it prevents a manually curated claim list from reporting a
high ratio while whole authorization, state-machine, concurrency, durability,
or secret-handling modules are absent from the denominator discussion.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys


ROOT = pathlib.Path(__file__).resolve().parents[2]
LEDGER = ROOT / "formal" / "coverage.json"
EXCLUSIONS = ROOT / "formal" / "surface-exclusions.json"


# Each category is a disjunction of conjunctions.  This keeps a lone word in a
# comment or DTO from becoming a safety surface while retaining every concrete
# decision, state transition, synchronization, durable fence, and plaintext
# boundary.  For example, a revision field alone is data; revision + CAS/fence
# logic is a formalizable safety relation.
SIGNAL_GROUPS: dict[str, tuple[tuple[re.Pattern[str], ...], ...]] = {
    "authorization": (
        (re.compile(r"\b(?:authorize|authorization|authz|authenticate)\w*\b", re.I),),
        (re.compile(r"\bpermission\w*\b", re.I),),
        (
            re.compile(r"\b(?:scope|tenant|workspace)[A-Za-z0-9_]*\b", re.I),
            re.compile(r"\b(?:resolve|guard|fence|admit|claim|allow|deny)\w*\b", re.I),
        ),
    ),
    "state_machine": (
        (
            re.compile(
                r"\benum\s+[A-Za-z0-9_]*(?:State|Phase|Status|Outcome|Disposition|Lifecycle)\b"
            ),
        ),
        (re.compile(r"\bfn\s+[A-Za-z0-9_]*(?:transition|next_state|advance|settle)\w*\b", re.I),),
        (
            re.compile(r"\b(?:terminal|absorbing)\w*\b", re.I),
            re.compile(r"\b(?:state|phase|status|outcome)\w*\b", re.I),
        ),
    ),
    "concurrency": (
        (re.compile(r"\b(?:Mutex|RwLock|Atomic[A-Za-z0-9_]*|compare_exchange)\b"),),
        (re.compile(r"\b(?:tokio::select|loom::|spawn_blocking|JoinSet)\b"),),
    ),
    "durability": (
        (
            re.compile(
                r"\b(?:transaction|begin_mutation|outbox|checkpoint|idempoten)\w*\b",
                re.I,
            ),
        ),
        (
            re.compile(r"\b(?:lease|revision|generation)\w*\b", re.I),
            re.compile(r"\b(?:claim|compare|fence|cas|stale|mutat|commit|renew)\w*\b", re.I),
        ),
        (
            re.compile(r"\b(?:recover|retry|reconcile)\w*\b", re.I),
            re.compile(r"\b(?:state|durable|pending|queue|outbox|checkpoint)\w*\b", re.I),
        ),
        (re.compile(r"\b(?:INSERT|UPDATE|DELETE)\s+(?:INTO|FROM)?\b", re.I),),
    ),
    "secret_boundary": (
        (re.compile(r"\b(?:PlaintextHolder|PlaintextBoundary)\b"),),
        (
            re.compile(r"\b(?:credential|secret|bearer|token|material)\w*\b", re.I),
            re.compile(
                r"\b(?:resolve|materializ|validate|rotate|revoke|redact|inject|holder|boundary)\w*\b",
                re.I,
            ),
        ),
    ),
}

PRODUCTION_ROOTS = (
    ROOT / "crates" / "contract",
    ROOT / "crates" / "control",
    ROOT / "crates" / "resources",
    ROOT / "crates" / "runtime",
    ROOT / "crates" / "server",
    ROOT / "crates" / "stores",
    ROOT / "crates" / "worker",
    ROOT / "crates" / "bin",
)

PROOF_STATUSES = {"machine_linked", "kernel_proved"}


def fail(message: str) -> None:
    print(f"formal surface: {message}", file=sys.stderr)
    raise SystemExit(1)


def is_production_source(path: pathlib.Path) -> bool:
    relative = path.relative_to(ROOT)
    parts = relative.parts
    if "src" not in parts:
        return False
    if path.name in {"tests.rs", "test.rs", "formal.rs"}:
        return False
    return not any(part in {"tests", "test_support", "fixtures"} for part in parts)


def code_without_comments(source: str) -> str:
    source = re.sub(r"/\*.*?\*/", "", source, flags=re.DOTALL)
    return "\n".join(line.split("//", 1)[0] for line in source.splitlines())


def detect_signals(source: str) -> list[str]:
    code = code_without_comments(source)
    return [
        category
        for category, groups in SIGNAL_GROUPS.items()
        if any(all(pattern.search(code) for pattern in group) for group in groups)
    ]


def load_exclusions() -> dict[str, dict[str, str]]:
    if not EXCLUSIONS.exists():
        return {}
    raw = json.loads(EXCLUSIONS.read_text(encoding="utf-8"))
    exclusions: dict[str, dict[str, str]] = {}
    for item in raw.get("exclusions", []):
        path = item.get("path", "")
        reason = item.get("reason", "")
        boundary = item.get("boundary", "")
        if not path or not reason or not boundary:
            fail("every surface exclusion needs path, boundary, and reason")
        if path in exclusions:
            fail(f"duplicate surface exclusion {path}")
        if not (ROOT / path).is_file():
            fail(f"surface exclusion references missing source {path}")
        exclusions[path] = item
    return exclusions


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--require-complete",
        action="store_true",
        help="fail when a detected production surface has no obligation or exclusion",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit the complete inventory as JSON",
    )
    args = parser.parse_args()

    ledger = json.loads(LEDGER.read_text(encoding="utf-8"))
    by_source: dict[str, list[dict[str, object]]] = {}
    for obligation in ledger["obligations"]:
        for evidence in obligation.get("evidence", []):
            if evidence.startswith("crates/") and evidence.endswith(".rs"):
                by_source.setdefault(evidence, []).append(obligation)

    exclusions = load_exclusions()
    inventory: list[dict[str, object]] = []
    for root in PRODUCTION_ROOTS:
        for path in root.glob("**/*.rs"):
            if not is_production_source(path):
                continue
            source = path.read_text(encoding="utf-8")
            signals = detect_signals(source)
            if not signals:
                continue
            relative = path.relative_to(ROOT).as_posix()
            obligations = by_source.get(relative, [])
            proved = [
                item["id"] for item in obligations if item["status"] in PROOF_STATUSES
            ]
            inventory.append(
                {
                    "path": relative,
                    "signals": signals,
                    "obligations": [item["id"] for item in obligations],
                    "proved_obligations": proved,
                    "excluded": exclusions.get(relative),
                }
            )

    inventory.sort(key=lambda item: str(item["path"]))
    paths = {str(item["path"]) for item in inventory}
    stale_exclusions = sorted(set(exclusions) - paths)
    if stale_exclusions:
        fail(
            "exclusions no longer match detected surfaces: "
            + ", ".join(stale_exclusions)
        )

    classified = [
        item for item in inventory if item["obligations"] or item["excluded"] is not None
    ]
    proof_linked = [item for item in inventory if item["proved_obligations"]]
    uncovered = [
        item for item in inventory if not item["obligations"] and item["excluded"] is None
    ]
    classified_ratio = len(classified) / len(inventory) if inventory else 1.0
    proof_ratio = len(proof_linked) / len(inventory) if inventory else 1.0

    if args.json:
        print(
            json.dumps(
                {
                    "summary": {
                        "inventoried": len(inventory),
                        "classified": len(classified),
                        "proof_linked": len(proof_linked),
                        "uncovered": len(uncovered),
                        "classified_ratio": classified_ratio,
                        "proof_linked_ratio": proof_ratio,
                    },
                    "surfaces": inventory,
                },
                indent=2,
                sort_keys=True,
            )
        )
    else:
        print(
            "formal source surface: "
            f"{len(classified)}/{len(inventory)} classified ({classified_ratio:.1%}); "
            f"{len(proof_linked)}/{len(inventory)} proof-linked ({proof_ratio:.1%}); "
            f"{len(uncovered)} uncovered"
        )
        for item in uncovered[:25]:
            print(f"  uncovered {item['path']} [{','.join(item['signals'])}]")
        if len(uncovered) > 25:
            print(f"  ... and {len(uncovered) - 25} more")

    if args.require_complete and uncovered:
        fail(f"{len(uncovered)} detected production safety surfaces are unclassified")


if __name__ == "__main__":
    main()
