"""One ledger-owned qualification policy shared by CI and release packaging.

This module classifies declared evidence; it does not run or authenticate proofs.
An eligible ledger is necessary, not sufficient, for a Verified deployment.
Target-guardrail profiles reference the same feature requirements and obligation
rows; they are diagnostic views, never a second source of guardrail status.
"""

import re

PROOF_GRADES = frozenset({
    "abstract_model_checked", "abstract_theorem_proved",
    "production_kernel_proved", "implementation_refined",
    "external_tcb", "semantic_nonclaim",
})
DIRECT_GRADES = frozenset({"production_kernel_proved", "implementation_refined"})
GUARDRAIL_ID = re.compile(r"G[1-9][0-9]*")
STALE_DIRECT_GRADE_CLAIM = re.compile(
    r"\b(?:the\s+)?(?:whole\s+)?(?:row|obligation)\b[^.]{0,160}(?:"
    r"\b(?:remains?|retains?|is)\b[^.]{0,80}"
    r"\b(?:abstract[-_ ]model(?:[-_ ]checked)?|model[-_ ]linked)\b"
    r"|\bis\s+not\s+upgraded\b)",
    re.IGNORECASE,
)


def validate_proof_grade(row: dict) -> None:
    key = row["id"]
    grade = row.get("proof_grade")
    if grade not in PROOF_GRADES:
        raise ValueError(f"{key} has unknown or missing proof_grade {grade!r}")
    if grade == "implementation_refined":
        # No universal Rust-to-model verifier is wired yet. Captured traces
        # remain evidence but cannot manufacture that missing capability.
        raise ValueError(f"{key} implementation_refined requires a checked universal production refinement; captured traces do not qualify")
    if grade == "production_kernel_proved" and not (row.get("proof_harnesses") or row.get("exhaustive_tests")):
        raise ValueError(f"{key} production-kernel grade has no Kani harness or exhaustive test")
    if grade in DIRECT_GRADES and not row.get("production_entrypoints"):
        raise ValueError(f"{key} direct production grade has no production entrypoint")
    if grade in DIRECT_GRADES and any(
        STALE_DIRECT_GRADE_CLAIM.search(bound)
        for bound in row.get("bounds", [])
        if isinstance(bound, str)
    ):
        raise ValueError(
            f"{key} direct production grade contradicts an obsolete abstract/model row claim"
        )


def resolve_obligations(requirement: dict, rows: dict, owner: str) -> set[str]:
    """Resolve explicit references and formalizable prefixes without widening."""
    explicit = requirement.get("obligations", [])
    prefixes = requirement.get("obligation_prefixes", [])
    if not all(isinstance(value, list) for value in (explicit, prefixes)):
        raise ValueError(f"{owner} obligation fields must be lists")
    selected = set()
    for obligation_id in explicit:
        if not isinstance(obligation_id, str) or obligation_id not in rows:
            raise ValueError(f"{owner} names missing obligation {obligation_id}")
        selected.add(obligation_id)
    for prefix in prefixes:
        if not isinstance(prefix, str) or not prefix:
            raise ValueError(f"{owner} has an invalid obligation prefix")
        matched = {
            key for key, row in rows.items()
            if key.startswith(f"{prefix}.") and row.get("formalizable") is True
        }
        if not matched:
            raise ValueError(f"{owner} prefix {prefix!r} matches no formalizable obligation")
        selected.update(matched)
    return selected


def direct_evidence_result(required: set[str], rows: dict) -> dict:
    """Classify one already-resolved repository-safety denominator."""
    if not required:
        raise ValueError("qualification cannot have an empty denominator")
    for key in required:
        row = rows[key]
        if row.get("formalizable") is not True or row.get("claim_scope") != "repository_safety":
            raise ValueError(f"{key} is not repository-controlled safety")
        validate_proof_grade(row)
    gaps = sorted(key for key in required if rows[key]["proof_grade"] not in DIRECT_GRADES)
    return {
        "required_obligations": len(required),
        "directly_proved_or_refined": len(required) - len(gaps),
        "eligible": not gaps,
        "gaps": gaps,
    }


def guardrail_results(features: dict, rows: dict) -> dict[str, dict]:
    """Resolve target guardrails through exact existing feature requirements."""
    profiles = features.get("guardrail_promotions", [])
    if not isinstance(profiles, list):
        raise ValueError("guardrail promotion profiles must be a list")
    if not profiles:
        return {}

    feature_rows = features.get("features")
    if not isinstance(feature_rows, list) or not feature_rows:
        raise ValueError("qualification requires nonempty features")
    feature_index = {}
    requirement_index = {}
    for feature in feature_rows:
        feature_id = feature.get("id")
        if not isinstance(feature_id, str) or not feature_id or feature_id in feature_index:
            raise ValueError("qualification requires unique nonempty feature ids")
        feature_index[feature_id] = feature
        for requirement in feature.get("requirements", []):
            requirement_id = requirement.get("id")
            coordinate = (feature_id, requirement_id)
            if not isinstance(requirement_id, str) or not requirement_id or coordinate in requirement_index:
                raise ValueError(f"{feature_id} requires unique nonempty requirement ids")
            requirement_index[coordinate] = requirement

    results = {}
    for profile in profiles:
        guardrail_id = profile.get("id")
        if (
            not isinstance(guardrail_id, str)
            or GUARDRAIL_ID.fullmatch(guardrail_id) is None
            or guardrail_id in results
        ):
            raise ValueError("qualification requires unique canonical guardrail ids")
        references = profile.get("requirements")
        if not isinstance(references, list) or not references:
            raise ValueError(f"{guardrail_id} requires at least one feature requirement")
        required = set()
        assumptions = set()
        seen = set()
        for reference in references:
            if not isinstance(reference, dict) or set(reference) != {"feature", "requirement"}:
                raise ValueError(f"{guardrail_id} has an invalid feature requirement reference")
            coordinate = (reference["feature"], reference["requirement"])
            if coordinate in seen:
                raise ValueError(f"{guardrail_id} repeats feature requirement {coordinate[0]}/{coordinate[1]}")
            seen.add(coordinate)
            requirement = requirement_index.get(coordinate)
            if requirement is None:
                raise ValueError(f"{guardrail_id} names missing feature requirement {coordinate[0]}/{coordinate[1]}")
            if requirement.get("kind") != "safety" or requirement.get("repository_controlled") is not True:
                raise ValueError(f"{guardrail_id} references non-repository-safety requirement {coordinate[0]}/{coordinate[1]}")
            selected = resolve_obligations(requirement, rows, f"{guardrail_id}:{coordinate[0]}")
            if not selected:
                raise ValueError(f"{guardrail_id} feature requirement {coordinate[0]}/{coordinate[1]} has no obligations")
            required.update(selected)
            declared_assumptions = requirement.get("assumptions", [])
            if not isinstance(declared_assumptions, list) or not all(
                isinstance(value, str) and value for value in declared_assumptions
            ):
                raise ValueError(f"{guardrail_id} has invalid deployment assumptions")
            assumptions.update(declared_assumptions)
        result = direct_evidence_result(required, rows)
        result["deployment_assumptions"] = sorted(assumptions)
        results[guardrail_id] = result
    return results


def profile_result(features: dict, coverage: dict) -> dict:
    obligations = coverage["obligations"]
    rows = {row["id"]: row for row in obligations}
    if not rows or len(rows) != len(obligations):
        raise ValueError("qualification requires unique, nonempty obligations")
    required = set()
    for feature in features["features"]:
        if feature.get("criticality") != "P0":
            continue
        for requirement in feature["requirements"]:
            if requirement.get("kind") != "safety" or requirement.get("repository_controlled") is not True:
                continue
            selected = resolve_obligations(requirement, rows, feature["id"])
            if not selected:
                raise ValueError(f"{feature['id']} P0 safety requirement has no obligations")
            required.update(selected)
    result = direct_evidence_result(required, rows)
    result["guardrails"] = guardrail_results(features, rows)
    return result
