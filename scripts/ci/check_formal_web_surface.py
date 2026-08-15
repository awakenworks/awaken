#!/usr/bin/env python3
"""Inventory product-significant TypeScript/TSX surfaces in ``web/src``.

The Rust formal-surface inventory intentionally scans only crates.  This
complementary inventory prevents browser authority, routing, storage, state,
request, and mutation code from disappearing from the product denominator.

A classification is a reviewed product-boundary link, not a formal proof.  In
particular, unit/browser tests and TypeScript type checking never make a row
``kernel_proved``.  A Web relation becomes formally proved only after a checked
proof obligation invokes a production proof kernel (for example shared
Rust/WASM) or a checked refinement trace links the browser behavior to it.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys


ROOT = pathlib.Path(__file__).resolve().parents[2]
WEB_ROOT = ROOT / "web" / "src"
LEDGER = ROOT / "formal" / "web-surface-classifications.json"
FEATURES = ROOT / "formal" / "features.json"


# Each category is a disjunction of regexes.  The scanner intentionally
# over-approximates: a false positive must be reviewed and classified rather
# than silently falling out of the product inventory.
SIGNALS: dict[str, tuple[re.Pattern[str], ...]] = {
    "authority": (
        re.compile(
            r"\b(?:authorization|authorize|permission|credential|secret|bearer|token)\w*\b",
            re.I,
        ),
        re.compile(r"\bworkspace(?:Id|_id)?\b", re.I),
        re.compile(r"\b(?:Gate|GatedPage|useGate)\b"),
    ),
    "route": (
        re.compile(
            r"\b(?:useParams|useNavigate|Navigate|RouterProvider|createBrowserRouter|RouteObject)\b"
        ),
        re.compile(
            r"\b(?:pathname|workspaceFromPath|workspaceIdForRequest|navPath|routeWorkspace)\b"
        ),
        re.compile(r"(?:^|[\"'`])/(?:v1|w)/"),
    ),
    "storage": (
        re.compile(r"\b(?:localStorage|sessionStorage|indexedDB)\b"),
        re.compile(r"\b(?:getItem|setItem|removeItem)\s*\("),
    ),
    "state": (
        re.compile(r"\b(?:useState|useReducer|createContext|useContext|useSyncExternalStore)\b"),
        re.compile(r"\b(?:queryClient|useQuery|useInfiniteQuery|setQueryData)\b"),
        re.compile(r"\b(?:State|Status|Phase|Outcome)\b"),
    ),
    "fetch": (
        re.compile(r"\bfetch\s*\("),
        re.compile(r"\bapi\.(?:get|post|put|del|upload|uploadMany|bytes|download)\s*[<(]"),
        re.compile(r"\b(?:EventSource|WebSocket)\b"),
    ),
    "mutation": (
        re.compile(r"\b(?:useMutation|mutateAsync|invalidateQueries)\b"),
        re.compile(r"\bapi\.(?:post|put|del|upload|uploadMany)\s*[<(]"),
        re.compile(r"\b(?:onSubmit|onSave|onDelete|onArchive|onCancel|setWorkspace)\b"),
    ),
}


def fail(message: str) -> None:
    print(f"formal web surface: {message}", file=sys.stderr)
    raise SystemExit(1)


def production_sources() -> list[pathlib.Path]:
    paths: list[pathlib.Path] = []
    for path in WEB_ROOT.rglob("*"):
        if path.suffix not in {".ts", ".tsx"} or not path.is_file():
            continue
        name = path.name
        if name.endswith(
            (".test.ts", ".test.tsx", ".spec.ts", ".spec.tsx", ".d.ts")
        ):
            continue
        paths.append(path)
    return sorted(paths)


def code_without_comments(source: str) -> str:
    source = re.sub(r"/\*.*?\*/", "", source, flags=re.DOTALL)
    return "\n".join(line.split("//", 1)[0] for line in source.splitlines())


def detect_signals(source: str) -> list[str]:
    code = code_without_comments(source)
    return [
        category
        for category, patterns in SIGNALS.items()
        if any(pattern.search(code) for pattern in patterns)
    ]


def requirement_ids() -> set[str]:
    raw = json.loads(FEATURES.read_text(encoding="utf-8"))
    return {
        f"{feature['id']}.{requirement['id']}"
        for feature in raw["features"]
        for requirement in feature["requirements"]
    }


def load_classifications(valid_requirements: set[str]) -> dict[str, dict[str, object]]:
    if not LEDGER.is_file():
        return {}
    raw = json.loads(LEDGER.read_text(encoding="utf-8"))
    if raw.get("version") != 1:
        fail("classification ledger version must be 1")
    rows = raw.get("classifications")
    if not isinstance(rows, list):
        fail("classification ledger must contain a classifications list")
    by_path: dict[str, dict[str, object]] = {}
    for row in rows:
        if not isinstance(row, dict):
            fail("every classification must be an object")
        path = row.get("path")
        requirement_id = row.get("requirement_id")
        reason = row.get("reason")
        proof_status = row.get("proof_status")
        required = (path, requirement_id, reason)
        if not all(isinstance(value, str) and value.strip() for value in required):
            fail("every classification needs non-empty path, requirement_id, and reason")
        if proof_status != "product_boundary_only":
            fail(f"{path}: proof_status must be product_boundary_only")
        if requirement_id not in valid_requirements:
            fail(f"{path}: unknown product requirement {requirement_id!r}")
        if path in by_path:
            fail(f"duplicate classification {path}")
        source = ROOT / path
        if not source.is_file():
            fail(f"classification references missing source {path}")
        if not source.is_relative_to(WEB_ROOT):
            fail(f"classification is outside web/src: {path}")
        by_path[path] = row
    return by_path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--json", action="store_true")
    parser.add_argument(
        "--inventory-only",
        action="store_true",
        help="print candidates without requiring the classification ledger",
    )
    args = parser.parse_args()

    inventory: list[dict[str, object]] = []
    for path in production_sources():
        signals = detect_signals(path.read_text(encoding="utf-8"))
        if signals:
            inventory.append(
                {
                    "path": path.relative_to(ROOT).as_posix(),
                    "signals": signals,
                }
            )

    classifications = (
        {} if args.inventory_only else load_classifications(requirement_ids())
    )
    candidates = {item["path"] for item in inventory}
    classified = set(classifications)
    uncovered = sorted(candidates - classified)
    stale = sorted(classified - candidates)

    result = {
        "candidate_count": len(candidates),
        "classified_count": len(candidates & classified),
        "uncovered": uncovered,
        "stale": stale,
        "inventory": inventory,
    }
    if args.json or args.inventory_only:
        print(json.dumps(result, indent=2, sort_keys=True))
    if args.inventory_only:
        return
    if uncovered:
        fail("uncovered candidates: " + ", ".join(uncovered))
    if stale:
        fail("classified files no longer carry a configured signal: " + ", ".join(stale))
    print(
        f"formal web surface: {len(candidates)}/{len(candidates)} candidates classified, "
        "0 uncovered; classifications are product boundaries, not proof claims"
    )


if __name__ == "__main__":
    main()
