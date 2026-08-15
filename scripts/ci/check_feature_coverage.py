#!/usr/bin/env python3
"""Validate the product-feature to verification-evidence ledger.

The formal obligation ledger is claim-oriented.  This checker supplies the
independent product denominator: every row in the canonical functional coverage
matrix must have exactly one machine-readable feature, and every formal
obligation or external assumption must be consumed by at least one requirement.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys


ROOT = pathlib.Path(__file__).resolve().parents[2]
FEATURES = ROOT / "formal" / "features.json"
ASSUMPTIONS = ROOT / "formal" / "assumptions.json"
COVERAGE = ROOT / "formal" / "coverage.json"
CRITICALITIES = {"P0", "P1", "P2"}
REQUIREMENT_KINDS = {"functional", "safety", "liveness", "environment"}
ASSUMPTION_KINDS = {
    "external_effect",
    "infrastructure",
    "liveness",
    "operational",
    "semantic",
    "supply_chain",
}
ASSUMPTION_STATUSES = {"open", "evidenced"}


def fail(message: str) -> None:
    print(f"feature coverage: {message}", file=sys.stderr)
    raise SystemExit(1)


def load_json(path: pathlib.Path) -> dict[str, object]:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read {path.relative_to(ROOT)}: {error}")


def require_path(relative: str, owner: str) -> None:
    if not relative or relative.startswith("/"):
        fail(f"{owner} has invalid repository-relative evidence {relative!r}")
    if not (ROOT / relative).is_file():
        fail(f"{owner} references missing evidence {relative}")


def functional_coverage_rows(relative: str) -> list[str]:
    path = ROOT / relative
    if not path.is_file():
        fail(f"coverage_matrix does not exist: {relative}")
    source = path.read_text(encoding="utf-8")
    try:
        table = source.split("## Functional Coverage Matrix", 1)[1].split(
            "## Reference Family Audit", 1
        )[0]
    except IndexError:
        fail(f"{relative} has no canonical Functional Coverage Matrix")
    rows = [
        line.split("|", 2)[1].strip()
        for line in table.splitlines()
        if line.startswith("| ")
    ]
    if not rows or rows[0] != "Requirement area":
        fail(f"{relative} has an unreadable Functional Coverage Matrix")
    return rows[1:]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--require-complete",
        action="store_true",
        help="fail unless every repository requirement has verification and every assumption is closed",
    )
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    feature_data = load_json(FEATURES)
    assumption_data = load_json(ASSUMPTIONS)
    coverage_data = load_json(COVERAGE)

    if feature_data.get("version") != 1:
        fail("formal/features.json version must be 1")
    if assumption_data.get("version") != 1:
        fail("formal/assumptions.json version must be 1")

    obligations = coverage_data.get("obligations")
    if not isinstance(obligations, list) or not obligations:
        fail("formal/coverage.json has no obligations")
    obligations_by_id = {str(item["id"]): item for item in obligations}
    if len(obligations_by_id) != len(obligations):
        fail("formal/coverage.json contains duplicate obligation ids")

    raw_assumptions = assumption_data.get("assumptions")
    if not isinstance(raw_assumptions, list):
        fail("formal/assumptions.json assumptions must be a list")
    assumptions: dict[str, dict[str, object]] = {}
    external_obligations = {
        obligation_id
        for obligation_id, obligation in obligations_by_id.items()
        if not obligation["formalizable"]
    }
    assumption_obligations: set[str] = set()
    for item in raw_assumptions:
        assumption_id = str(item.get("id", ""))
        if not assumption_id or assumption_id in assumptions:
            fail(f"duplicate or empty assumption id {assumption_id!r}")
        obligation_id = str(item.get("obligation", ""))
        if obligation_id not in external_obligations:
            fail(f"{assumption_id} does not name an external obligation")
        if obligation_id in assumption_obligations:
            fail(f"external obligation {obligation_id} has multiple assumptions")
        assumption_obligations.add(obligation_id)
        if item.get("kind") not in ASSUMPTION_KINDS:
            fail(f"{assumption_id} has invalid kind {item.get('kind')!r}")
        if item.get("status") not in ASSUMPTION_STATUSES:
            fail(f"{assumption_id} has invalid status {item.get('status')!r}")
        integration = item.get("integration_evidence", [])
        runtime = item.get("runtime_evidence", [])
        if not isinstance(integration, list) or not integration:
            fail(f"{assumption_id} needs integration_evidence")
        if not isinstance(runtime, list):
            fail(f"{assumption_id} runtime_evidence must be a list")
        for relative in integration:
            require_path(str(relative), assumption_id)
        for evidence in runtime:
            if not isinstance(evidence, dict):
                fail(f"{assumption_id} runtime evidence must be an object")
            kind = str(evidence.get("kind", ""))
            name = str(evidence.get("name", ""))
            relative = str(evidence.get("path", ""))
            if kind not in {"durable_state", "log", "metric", "state"}:
                fail(f"{assumption_id} has invalid runtime evidence kind {kind!r}")
            if not name:
                fail(f"{assumption_id} runtime evidence has no observable name")
            require_path(relative, assumption_id)
            if name not in (ROOT / relative).read_text(encoding="utf-8"):
                fail(
                    f"{assumption_id} runtime evidence {name!r} is absent from {relative}"
                )
        assumptions[assumption_id] = item
    if assumption_obligations != external_obligations:
        missing = sorted(external_obligations - assumption_obligations)
        extra = sorted(assumption_obligations - external_obligations)
        fail(f"external assumption mismatch; missing={missing}, extra={extra}")

    raw_features = feature_data.get("features")
    if not isinstance(raw_features, list) or not raw_features:
        fail("formal/features.json features must be a non-empty list")
    feature_ids: set[str] = set()
    requirement_areas: list[str] = []
    requirement_ids: set[str] = set()
    used_obligations: set[str] = set()
    used_assumptions: set[str] = set()
    incomplete_requirements: list[str] = []

    for feature in raw_features:
        feature_id = str(feature.get("id", ""))
        if not feature_id or feature_id in feature_ids:
            fail(f"duplicate or empty feature id {feature_id!r}")
        feature_ids.add(feature_id)
        area = str(feature.get("requirement_area", ""))
        if not area:
            fail(f"{feature_id} has no requirement_area")
        requirement_areas.append(area)
        if feature.get("criticality") not in CRITICALITIES:
            fail(f"{feature_id} has invalid criticality {feature.get('criticality')!r}")
        entrypoints = feature.get("entrypoints", [])
        if not isinstance(entrypoints, list) or not entrypoints:
            fail(f"{feature_id} must name at least one entrypoint")
        requirements = feature.get("requirements", [])
        if not isinstance(requirements, list) or not requirements:
            fail(f"{feature_id} must name at least one requirement")

        for requirement in requirements:
            local_id = str(requirement.get("id", ""))
            requirement_id = f"{feature_id}.{local_id}"
            if not local_id or requirement_id in requirement_ids:
                fail(f"duplicate or empty requirement id {requirement_id!r}")
            requirement_ids.add(requirement_id)
            if requirement.get("kind") not in REQUIREMENT_KINDS:
                fail(f"{requirement_id} has invalid kind {requirement.get('kind')!r}")
            if not isinstance(requirement.get("repository_controlled"), bool):
                fail(f"{requirement_id} must classify repository_controlled")

            evidence = requirement.get("evidence", [])
            if not isinstance(evidence, list) or not evidence:
                fail(f"{requirement_id} needs classification evidence")
            for relative in evidence:
                require_path(str(relative), requirement_id)

            explicit = requirement.get("obligations", [])
            prefixes = requirement.get("obligation_prefixes", [])
            references = requirement.get("assumptions", [])
            if not all(isinstance(value, list) for value in (explicit, prefixes, references)):
                fail(f"{requirement_id} obligation and assumption fields must be lists")

            matched: set[str] = set()
            for obligation_id in explicit:
                obligation_id = str(obligation_id)
                if obligation_id not in obligations_by_id:
                    fail(f"{requirement_id} names missing obligation {obligation_id}")
                matched.add(obligation_id)
            for prefix in prefixes:
                prefix = str(prefix)
                prefix_matches = {
                    obligation_id
                    for obligation_id in obligations_by_id
                    if obligation_id.startswith(f"{prefix}.")
                    and obligations_by_id[obligation_id]["formalizable"]
                }
                if not prefix_matches:
                    fail(f"{requirement_id} prefix {prefix!r} matches no formalizable obligation")
                matched.update(prefix_matches)
            used_obligations.update(matched)

            for assumption_id in references:
                assumption_id = str(assumption_id)
                if assumption_id not in assumptions:
                    fail(f"{requirement_id} names missing assumption {assumption_id}")
                used_assumptions.add(assumption_id)

            has_verification = bool(matched or requirement.get("executable_evidence"))
            if requirement["repository_controlled"] and not has_verification:
                incomplete_requirements.append(requirement_id)

    matrix = functional_coverage_rows(str(feature_data.get("coverage_matrix", "")))
    if len(requirement_areas) != len(set(requirement_areas)):
        fail("multiple features map to the same functional coverage row")
    missing_areas = sorted(set(matrix) - set(requirement_areas))
    extra_areas = sorted(set(requirement_areas) - set(matrix))
    if missing_areas or extra_areas:
        fail(f"functional matrix mismatch; missing={missing_areas}, extra={extra_areas}")

    formalizable_obligations = {
        obligation_id
        for obligation_id, obligation in obligations_by_id.items()
        if obligation["formalizable"]
    }
    orphan_obligations = sorted(formalizable_obligations - used_obligations)
    orphan_assumptions = sorted(set(assumptions) - used_assumptions)
    if orphan_obligations:
        fail(f"formal obligations have no product requirement: {', '.join(orphan_obligations)}")
    if orphan_assumptions:
        fail(f"external assumptions have no product requirement: {', '.join(orphan_assumptions)}")

    open_assumptions = sorted(
        assumption_id
        for assumption_id, item in assumptions.items()
        if item["status"] != "evidenced"
        or (
            item["kind"] in {"liveness", "operational"}
            and not item.get("runtime_evidence")
        )
    )
    summary = {
        "features": len(feature_ids),
        "requirements": len(requirement_ids),
        "formalizable_obligations": len(formalizable_obligations),
        "linked_obligations": len(used_obligations),
        "external_assumptions": len(assumptions),
        "incomplete_requirements": len(incomplete_requirements),
        "open_assumptions": len(open_assumptions),
    }
    if args.json:
        print(
            json.dumps(
                {
                    "summary": summary,
                    "incomplete_requirements": sorted(incomplete_requirements),
                    "open_assumptions": open_assumptions,
                },
                indent=2,
                sort_keys=True,
            )
        )
    else:
        print(
            "product verification coverage: "
            f"{summary['features']} features / {summary['requirements']} requirements; "
            f"{summary['linked_obligations']}/{summary['formalizable_obligations']} obligations linked; "
            f"{summary['incomplete_requirements']} requirements without verification; "
            f"{summary['open_assumptions']} external assumptions open"
        )
    if args.require_complete and (incomplete_requirements or open_assumptions):
        fail(
            "completion required but gaps remain: "
            f"requirements={len(incomplete_requirements)}, assumptions={len(open_assumptions)}"
        )


if __name__ == "__main__":
    main()
