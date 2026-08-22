use awaken_runtime_contract::{
    AttemptCredentialBinding, AttemptCredentialBindingError, CredentialRealizationCapabilities,
};
use awaken_worker_contract::WorkerSnapshot;

use crate::run_dispatch::RunDispatch;

/// Complete claim-time credential admission failure for one dispatch.
///
/// Inference failures retain the Runtime contract's neutral vocabulary. Session
/// MCP projection failures belong here because durable Run ingress is the only
/// boundary that joins the frozen Session envelope to Worker claim admission.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DispatchCredentialAdmissionError {
    #[error(transparent)]
    Attempt(#[from] AttemptCredentialBindingError),
    #[error("Session runtime credential projection is invalid: {0}")]
    InvalidSessionCredentialProjection(String),
    #[error("Session MCP credential and selected plaintext holder must be present together")]
    InvalidSessionMcpCredentialBinding,
    #[error("Session MCP credential usage is unsupported")]
    InvalidSessionMcpCredentialUsage,
    #[error("Session MCP credentials require a Worker plaintext holder")]
    UnsupportedSessionMcpHolder,
    #[error("Session MCP credential admission failed: {0}")]
    SessionAdmission(awaken_runtime_contract::CredentialAdmissionError),
}

pub fn worker_credential_realization_capabilities(
    worker: &WorkerSnapshot,
) -> Result<CredentialRealizationCapabilities, AttemptCredentialBindingError> {
    CredentialRealizationCapabilities::from_manifest_capabilities(&worker.manifest.capabilities)
        .map_err(AttemptCredentialBindingError::InvalidWorkerCapabilities)
}

/// Compile all credential-bearing candidates selected for this Run into exact
/// attempt bindings. Every caller must pass installed capability evidence: an
/// immutable registered Worker manifest or the in-process Worker's composed
/// capabilities. Claim admission never synthesizes capabilities from the request.
pub fn compile_attempt_credential_bindings(
    request: &RunDispatch,
    installed: &CredentialRealizationCapabilities,
    claim_epoch: u64,
    now_unix_ms: u64,
) -> Result<Vec<AttemptCredentialBinding>, DispatchCredentialAdmissionError> {
    let candidates = request
        .activation
        .snapshot
        .resolved_spec
        .attempt_candidates(request.activation.model_ref_override.as_deref());
    let bindings = awaken_runtime_contract::compile_candidate_credential_bindings(
        &candidates,
        request.inference_plaintext_holder.as_ref(),
        installed,
        claim_epoch,
        now_unix_ms,
    )?;
    if let Some(envelope) = &request.session_runtime {
        let projection = envelope.decode_projection().map_err(|error| {
            DispatchCredentialAdmissionError::InvalidSessionCredentialProjection(error.to_string())
        })?;
        for stage in projection.mcp_stages.into_iter().flatten() {
            match (
                stage.credential.as_ref(),
                stage.selected_plaintext_holder.as_ref(),
            ) {
                (None, None) => {}
                (Some(access), Some(holder)) => {
                    if holder.boundary != awaken_runtime_contract::PlaintextBoundary::Worker {
                        return Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder);
                    }
                    match &access.usage {
                        awaken_runtime_contract::CredentialUsage::HttpHeader { name, scheme }
                            if name.eq_ignore_ascii_case("authorization")
                                && scheme.as_deref().is_some_and(|scheme| {
                                    scheme.eq_ignore_ascii_case("bearer")
                                }) => {}
                        _ => {
                            return Err(
                                DispatchCredentialAdmissionError::InvalidSessionMcpCredentialUsage,
                            );
                        }
                    }
                    access
                        .admit(
                            holder,
                            awaken_runtime_contract::CredentialRealizationKind::WorkerRelay,
                            installed,
                            now_unix_ms,
                        )
                        .map_err(DispatchCredentialAdmissionError::SessionAdmission)?;
                }
                _ => {
                    return Err(
                        DispatchCredentialAdmissionError::InvalidSessionMcpCredentialBinding,
                    );
                }
            }
        }
    }
    Ok(bindings)
}

/// Read-only eligibility check for a scheduler selecting among multiple rows.
/// Exact claim still calls [`compile_attempt_credential_bindings`] and returns
/// the concrete failure; a broad selector skips a row this Worker cannot admit
/// so it cannot poison unrelated runnable work.
#[must_use]
pub fn can_admit_attempt_credentials(
    request: &RunDispatch,
    installed: &CredentialRealizationCapabilities,
    claim_epoch: u64,
    now_unix_ms: u64,
) -> bool {
    compile_attempt_credential_bindings(request, installed, claim_epoch, now_unix_ms).is_ok()
}
