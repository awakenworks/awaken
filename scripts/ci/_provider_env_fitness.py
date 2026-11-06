"""Keep the GenAI transport adapter on transport-ready facts.

Environment reads moved to the compiler: the workspace `clippy.toml` denies
`std::env::{var, var_os, vars, vars_os}` in production code (R11). The ambient
SDK default moved to Cargo (R12): `deny.toml` admits a direct `genai`
dependency only for `awaken-provider-genai` and the devtools example, so no
other crate can name `genai::Client` (or its `Default`), and `GenaiExecutor`
has no `Default`/`new`. One rule stays here because no tool can name it: the
GenAI transport adapter consumes only transport-ready facts (no Control,
access, custody, placement or scope vocabulary) and exposes one materialized
constructor.

Retires when the transport rule moves into the adapter's types.
"""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace


GENAI_PARALLEL_CONSTRUCTOR = re.compile(
    r"(?:pub\s+fn\s+(?:anthropic_compatible|vertex_gemini|new_for_provider)\b|"
    r"pub\s+fn\s+new\s*\(|from_materialized_api_key\b)"
)

GENAI_CONTROL_TYPES = {
    "AttemptCredentialBinding",
    "AttemptCredentialRealization",
    "BrokeredInferenceClient",
    "CandidateFingerprint",
    "CredentialAccess",
    "CredentialBinding",
    "CredentialCustodyPublication",
    "CredentialExecutionPolicy",
    "CredentialMaterialBinding",
    "CredentialMaterialDelivery",
    "CredentialRealizationCapabilities",
    "CredentialRealizationKind",
    "CredentialRealizationPlan",
    "CredentialRealizationProfile",
    "CredentialRef",
    "CredentialUsage",
    "ExactCredentialAccessRequest",
    "InferencePlacement",
    "ModelProvisioning",
    "Offering",
    "OfferingSource",
    "PlaintextBoundary",
    "PlaintextHolder",
    "ProviderAccessKind",
    "ProviderCatalog",
    "ProviderExecutionProfile",
    "ResolvedModelCandidate",
    "ScopeId",
    "WorkspaceScope",
}
GENAI_CONTROL_FIELDS = {
    "access_kind",
    "actual_realization_kind",
    "candidate_fingerprint",
    "claim_epoch",
    "credential_binding",
    "credential_id",
    "credential_pool_id",
    "credential_ref",
    "credential_source",
    "credential_usage",
    "custody",
    "directory_id",
    "funding_source",
    "location",
    "organization_id",
    "oauth_token",
    "plaintext_boundary",
    "processing_placement",
    "product_id",
    "project_id",
    "provider_ref",
    "realization_kind",
    "route_ref",
    "selected_plaintext_holder",
    "selected_realization_kind",
    "scope_id",
    "workspace_id",
}


def _genai_transport_errors(repo_root: Path, crates: Path) -> list[str]:
    """GenAI receives only transport-ready protocol, endpoint, model and key facts."""

    errors: list[str] = []
    manifest = _crate_boundary_workspace.crate_manifest("awaken-provider-genai", crates)
    if manifest is None:
        return ["awaken-provider-genai: the GenAI transport adapter this rule guards is missing"]
    source = manifest.parent / "src"
    banned = GENAI_CONTROL_TYPES | GENAI_CONTROL_FIELDS
    for path in source.glob("**/*.rs"):
        text = path.read_text(encoding="utf-8")
        if GENAI_PARALLEL_CONSTRUCTOR.search(text):
            errors.append(
                f"{path.relative_to(repo_root)}: GenAI transport adapter must expose one "
                "materialized endpoint constructor; provider/access-specific constructors "
                "would create a parallel route"
            )
        for token in sorted(banned):
            if re.search(rf"\b{re.escape(token)}\b", text):
                errors.append(
                    f"{path.relative_to(repo_root)}: GenAI transport adapter must not consume "
                    f"publication, access, custody, placement, or scope fact `{token}`; "
                    "the credential materializer must project transport-ready inputs"
                )
    return errors


def check_all(repo_root: Path, crates: Path) -> list[str]:
    """Reject control-plane vocabulary and parallel constructors in the adapter."""

    return _genai_transport_errors(repo_root, crates)


def selftest() -> None:
    """Exercise both sides of the provider transport boundary decision table."""

    import tempfile

    # Cause/effect design: C1 a GenAI source names any Control/access/custody
    # type or field => E1 reject it; C2 source contains only materialized
    # transport inputs => E2 accept it. Constraint: the check is scoped to the
    # one provider adapter and must not ban publication facts from their owner.
    # Rules: T1=C1=>E1 for every banned token; T2=C2=>E2.
    with tempfile.TemporaryDirectory() as directory:
        repo_root = Path(directory)
        crates = repo_root / "crates"
        source = crates / "sdk" / "providers" / "awaken-provider-genai" / "src"
        source.mkdir(parents=True)
        target = source / "lib.rs"
        _crate_boundary_workspace.ensure_fixture_manifest(target)
        for token in sorted(GENAI_CONTROL_TYPES | GENAI_CONTROL_FIELDS):
            target.write_text(f"{token}\n", encoding="utf-8")
            errors = _genai_transport_errors(repo_root, crates)
            assert len(errors) == 1 and f"`{token}`" in errors[0], (token, errors)

        target.write_text(
            "AdapterKind api_dialect base_url upstream_model api_key\n",
            encoding="utf-8",
        )
        assert not _genai_transport_errors(repo_root, crates)

        for constructor in [
            "pub fn anthropic_compatible",
            "pub fn vertex_gemini",
            "pub fn new()",
            "pub fn new_for_provider",
            "from_materialized_api_key",
        ]:
            target.write_text(constructor, encoding="utf-8")
            errors = _genai_transport_errors(repo_root, crates)
            assert len(errors) == 1 and "parallel route" in errors[0], (
                constructor,
                errors,
            )
