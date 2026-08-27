"""Keep ambient product configuration outside shipped execution paths."""

from __future__ import annotations

import re
from pathlib import Path


PROVIDER_ENV_READ = re.compile(
    r"std::env::var(?:_os)?\(\s*\"(?:"
    r"ANTHROPIC_|OPENAI_|KIMI_|GEMINI_|MINIMAX_|"
    r"AWAKEN_MODEL_FALLBACKS|AWAKEN_ACP_GATEWAY_|"
    r"AWAKEN_ACP_LEASE_|AWAKEN_ACP_CREDENTIAL_FILE"
    r"|PODMAN_BIN|AWAKEN_BASH|AWAKEN_SANDBOX_AGENT_STDERR"
    r"|AWAKEN_MEMORY_(?:STORE_ID|MOUNT_PATH|STORE_DIR|MODE|SHUTDOWN_)"
    r"|AWAKEN_LOG_FORMAT|AWAKEN_TRACE_FILE|RUST_LOG"
    r"|OTEL_EXPORTER_|OTEL_SERVICE_|OTEL_METRIC_EXPORT_INTERVAL"
    r")"
)

AMBIENT_SDK_CONSTRUCTOR = re.compile(
    r"(?:GenaiExecutor::(?:new|default)|genai::Client::default)\s*\("
)

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
    source = crates / "runtime" / "awaken-provider-genai" / "src"
    if not source.exists():
        return errors
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
    """Reject direct provider-environment reads in shipped Rust code.

    Devtools and test targets remain explicit fixtures and are never product
    composition roots.
    """

    errors: list[str] = []
    for path in crates.glob("**/*.rs"):
        relative = path.relative_to(crates)
        if relative.parts[0] == "devtools" or "tests" in relative.parts:
            continue
        if PROVIDER_ENV_READ.search(path.read_text(encoding="utf-8")):
            errors.append(
                f"{path.relative_to(repo_root)}: provider execution configuration "
                "must come from persisted catalog/credential/deployment policy"
            )
        if AMBIENT_SDK_CONSTRUCTOR.search(path.read_text(encoding="utf-8")):
            errors.append(
                f"{path.relative_to(repo_root)}: ambient provider SDK defaults are "
                "forbidden; construct the adapter from published endpoint and credential facts"
            )
    return errors + _genai_transport_errors(repo_root, crates)


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
        source = crates / "runtime" / "awaken-provider-genai" / "src"
        source.mkdir(parents=True)
        target = source / "lib.rs"
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
