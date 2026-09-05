"""One ledger-owned qualification policy shared by CI and release packaging.

This module classifies declared evidence; it does not run or authenticate proofs.
An eligible ledger is necessary, not sufficient, for a Verified deployment.
"""

PROOF_GRADES = frozenset({
    "abstract_model_checked", "abstract_theorem_proved",
    "production_kernel_proved", "implementation_refined",
    "external_tcb", "semantic_nonclaim",
})
DIRECT_GRADES = frozenset({"production_kernel_proved", "implementation_refined"})


def validate_proof_grade(row: dict) -> None:
    key = row["id"]
    grade = row.get("proof_grade")
    if grade not in PROOF_GRADES:
        raise ValueError(f"{key} has unknown or missing proof_grade {grade!r}")
    if grade == "implementation_refined":
        # No universal Rust-to-model verifier is wired yet. Captured traces
        # remain evidence but cannot manufacture that missing capability.
        raise ValueError(f"{key} implementation_refined requires a checked universal production refinement; captured traces do not qualify")
    if grade == "production_kernel_proved" and not row.get("proof_harnesses"):
        raise ValueError(f"{key} production-kernel grade has no Kani harness")
    if grade in DIRECT_GRADES and not row.get("production_entrypoints"):
        raise ValueError(f"{key} direct production grade has no production entrypoint")


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
            for key in selected:
                if rows[key].get("formalizable") is not True or rows[key].get("claim_scope") != "repository_safety":
                    raise ValueError(f"{key} is not repository-controlled safety")
            required.update(selected)
    if not required:
        raise ValueError("Verified qualification cannot have an empty P0 denominator")
    for key in required:
        validate_proof_grade(rows[key])
    gaps = sorted(key for key in required if rows[key]["proof_grade"] not in DIRECT_GRADES)
    return {
        "required_obligations": len(required),
        "directly_proved_or_refined": len(required) - len(gaps),
        "eligible": not gaps,
        "gaps": gaps,
    }
