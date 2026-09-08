//! Shared exact-adapter admission rules.
//!
//! The materializer remains the sole realization adapter; this module only
//! centralizes its immutable target and capability checks.

use awaken_runtime_contract::{
    CredentialAccess, CredentialAdmissionError, CredentialRealizationCapabilities,
    CredentialRealizationKind, PlaintextHolder,
};

#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "authority"), allow(dead_code))]
pub(super) struct ExpectedProviderTarget<'a> {
    pub(super) provider_id: &'a str,
    pub(super) protocol_endpoint_id: Option<&'a str>,
}

pub(super) fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// Validate one explicit adapter mechanism without introducing a capability
/// search. The caller is the installed adapter; this function only prevents its
/// Native, ACP, MCP, and Resource entry points from drifting in admission rules.
pub(super) fn admit_exact_adapter(
    access: &CredentialAccess,
    selected_holder: &PlaintextHolder,
    realization: CredentialRealizationKind,
    capabilities: &CredentialRealizationCapabilities,
) -> Result<(), CredentialAdmissionError> {
    access.admit(selected_holder, realization, capabilities, unix_time_ms())?;
    Ok(())
}
