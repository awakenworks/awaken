// @formal surface obligation=credential_reference.exact_revision_never_resolves_another_revision
// @formal surface obligation=worker.revoked_credential_blocks_new_materialization
// @formal surface obligation=worker.durable_dispatch_contains_no_plaintext_secret
// @formal reason: The sole materializer's shared admission helper rechecks the selected plaintext holder,
// @formal reason: exact source mechanism, expiry, and immutable target before any secret realization.
//! Shared exact-adapter admission rules.
//!
//! The materializer remains the sole realization adapter; this module only
//! centralizes its immutable target and capability checks.

use awaken_credential_contract::{
    CredentialAdmissionError, CredentialRealizationCapabilities, CredentialRealizationKind,
};
use awaken_runtime_contract::{CredentialAccess, PlaintextHolder};

pub(super) use awaken_credential_contract::CredentialProviderExpectation as ExpectedProviderTarget;

/// Validate one explicit adapter mechanism without introducing a capability
/// search. The caller is the installed adapter; this function only prevents its
/// Native, ACP, MCP, and Resource entry points from drifting in admission rules.
pub(super) fn admit_exact_adapter(
    access: &CredentialAccess,
    selected_holder: &PlaintextHolder,
    realization: CredentialRealizationKind,
    capabilities: &CredentialRealizationCapabilities,
    now_unix_ms: u64,
) -> Result<(), CredentialAdmissionError> {
    access.admit(selected_holder, realization, capabilities, now_unix_ms)?;
    Ok(())
}
