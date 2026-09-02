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
import re
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
EXECUTABLE_EVIDENCE_KINDS = {"ci_checker", "e2e_scenario", "rust_test"}


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


def require_executable_evidence(evidence: object, owner: str) -> None:
    """Require an executable, named oracle rather than accepting a bare path."""
    if not isinstance(evidence, dict):
        fail(f"{owner} executable evidence must be an object")
    kind = str(evidence.get("kind", ""))
    name = str(evidence.get("name", ""))
    relative = str(evidence.get("path", ""))
    if kind not in EXECUTABLE_EVIDENCE_KINDS:
        fail(f"{owner} has invalid executable evidence kind {kind!r}")
    if not name:
        fail(f"{owner} executable evidence has no test/check name")
    require_path(relative, owner)
    path = ROOT / relative
    source = path.read_text(encoding="utf-8")
    if name not in source:
        fail(f"{owner} executable evidence {name!r} is absent from {relative}")

    if kind == "rust_test":
        if path.suffix != ".rs":
            fail(f"{owner} Rust test evidence must reference a .rs file")
        test = re.compile(
            r"#\[(?:tokio::)?test(?:\([^\]]*\))?\]\s*"
            r"(?:#\[[^\]]+\]\s*)*(?:async\s+)?fn\s+"
            + re.escape(name)
            + r"\s*\("
        )
        if not test.search(source):
            fail(f"{owner} names {name!r}, but it is not a Rust test in {relative}")
    elif kind == "e2e_scenario":
        if not relative.startswith("e2e/") or path.suffix not in {".js", ".mjs", ".ts"}:
            fail(f"{owner} E2E evidence must reference an e2e JS/TS scenario")
        package = load_json(ROOT / "e2e" / "package.json")
        scripts = package.get("scripts", {})
        if not isinstance(scripts, dict) or not any(
            path.name in str(command) for command in scripts.values()
        ):
            fail(f"{owner} E2E evidence {relative} is not package-script orchestrated")
        has_entrypoint = bool(re.search(r"\bmain\s*\(\s*\)", source)) or (
            "import assert" in source and "console.log" in source
        )
        if "assert" not in source or not has_entrypoint:
            fail(f"{owner} E2E evidence {relative} has no executable entrypoint/assert oracle")
    else:
        if not relative.startswith("scripts/ci/") or path.suffix != ".py":
            fail(f"{owner} CI checker evidence must reference a scripts/ci Python checker")
        callable_pattern = re.compile(r"^def\s+" + re.escape(name) + r"\s*\(", re.MULTILINE)
        if not callable_pattern.search(source) or "__main__" not in source:
            fail(f"{owner} CI checker {name!r} is not an executable checker callable")


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
    feature_obligations: dict[str, set[str]] = {}
    feature_assumptions: dict[str, set[str]] = {}
    feature_has_executable: dict[str, bool] = {}

    for feature in raw_features:
        feature_id = str(feature.get("id", ""))
        if not feature_id or feature_id in feature_ids:
            fail(f"duplicate or empty feature id {feature_id!r}")
        feature_ids.add(feature_id)
        feature_obligations[feature_id] = set()
        feature_assumptions[feature_id] = set()
        feature_has_executable[feature_id] = False
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
            feature_obligations[feature_id].update(matched)

            for assumption_id in references:
                assumption_id = str(assumption_id)
                if assumption_id not in assumptions:
                    fail(f"{requirement_id} names missing assumption {assumption_id}")
                used_assumptions.add(assumption_id)
                feature_assumptions[feature_id].add(assumption_id)

            executable = requirement.get("executable_evidence", [])
            if not isinstance(executable, list):
                fail(f"{requirement_id} executable_evidence must be a list")
            for item in executable:
                require_executable_evidence(item, requirement_id)
            feature_has_executable[feature_id] |= bool(executable)

            has_verification = bool(matched or executable)
            if requirement["repository_controlled"] and not has_verification:
                incomplete_requirements.append(requirement_id)

    # Product journeys form an end-to-end assurance graph over the canonical
    # feature denominator. They add no second requirements ledger: every stage
    # names one existing feature and every anchor names a formal obligation
    # already consumed by a requirement in that journey.
    raw_journeys = feature_data.get("product_journeys")
    if not isinstance(raw_journeys, list) or not raw_journeys:
        fail("formal/features.json must define a non-empty product_journeys assurance graph")
    journey_ids: set[str] = set()
    journey_features: set[str] = set()
    journey_obligations: set[str] = set()
    journey_assumptions: set[str] = set()
    for journey in raw_journeys:
        if not isinstance(journey, dict):
            fail("product journey must be an object")
        journey_id = str(journey.get("id", ""))
        if not journey_id or journey_id in journey_ids:
            fail(f"duplicate or empty product journey id {journey_id!r}")
        journey_ids.add(journey_id)
        if not str(journey.get("objective", "")).strip():
            fail(f"{journey_id} has no objective")
        stages = journey.get("stages")
        if not isinstance(stages, list) or len(stages) < 2:
            fail(f"{journey_id} must connect at least two product stages")
        if len(stages) != len(set(stages)):
            fail(f"{journey_id} repeats a product stage")
        unknown_stages = sorted(set(stages) - feature_ids)
        if unknown_stages:
            fail(f"{journey_id} names unknown product stages: {unknown_stages}")
        journey_features.update(str(stage) for stage in stages)

        anchors = journey.get("formal_anchors")
        if not isinstance(anchors, list) or not anchors:
            fail(f"{journey_id} must name at least one formal composition anchor")
        stage_obligations = set().union(
            *(feature_obligations[str(stage)] for stage in stages)
        )
        journey_obligations.update(stage_obligations)
        journey_assumptions.update(
            set().union(*(feature_assumptions[str(stage)] for stage in stages))
        )
        for anchor in anchors:
            anchor = str(anchor)
            obligation = obligations_by_id.get(anchor)
            if obligation is None or not obligation["formalizable"]:
                fail(f"{journey_id} names missing or external formal anchor {anchor}")
            if anchor not in stage_obligations:
                fail(
                    f"{journey_id} formal anchor {anchor} is not consumed by any journey stage"
                )

        for stage in stages:
            stage = str(stage)
            if not (
                feature_obligations[stage]
                or feature_assumptions[stage]
                or feature_has_executable[stage]
            ):
                fail(f"{journey_id} stage {stage} has no assurance evidence")

    disconnected_features = sorted(feature_ids - journey_features)
    if disconnected_features:
        fail(
            "product features are disconnected from every assurance journey: "
            + ", ".join(disconnected_features)
        )

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
    disconnected_obligations = sorted(formalizable_obligations - journey_obligations)
    if disconnected_obligations:
        fail(
            "formal obligations are disconnected from every product journey: "
            + ", ".join(disconnected_obligations)
        )
    disconnected_assumptions = sorted(set(assumptions) - journey_assumptions)
    if disconnected_assumptions:
        fail(
            "external assumptions are disconnected from every product journey: "
            + ", ".join(disconnected_assumptions)
        )
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
        "product_journeys": len(journey_ids),
        "journey_connected_features": len(journey_features),
        "journey_linked_obligations": len(journey_obligations),
        "journey_linked_assumptions": len(journey_assumptions),
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
