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
    #[error(
        "Session MCP credential delivery is unsupported for the selected holder or ACP backend"
    )]
    UnsupportedSessionMcpHolder,
    #[error("Session MCP credential admission failed: {0}")]
    SessionAdmission(awaken_runtime_contract::CredentialAdmissionError),
    #[error("claimed credential bindings differ from the exact dispatch claim decision")]
    ClaimedBindingMismatch,
}

/// The one claim-scoped credential purpose selected by the dispatch aggregate.
///
/// Execution admits every candidate and Session attachment that may run under
/// the claim. Cancellation is narrower: only an authenticated remote A2A
/// transport can require material in order to observe or stop an already
/// published remote task. Terminal repair performs no external operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimCredentialOperation {
    Execute,
    Cancel,
    TerminalResolution,
}

/// Classify the claim action once for every persistence adapter.
///
/// Reservation repair is terminal control even if an old row also carries a
/// cancellation bit, so it deliberately takes precedence over cancellation.
#[must_use]
pub const fn claim_credential_operation(
    terminal_resolution: bool,
    cancellation_requested: bool,
) -> ClaimCredentialOperation {
    if terminal_resolution {
        ClaimCredentialOperation::TerminalResolution
    } else if cancellation_requested {
        ClaimCredentialOperation::Cancel
    } else {
        ClaimCredentialOperation::Execute
    }
}

#[cfg(kani)]
#[kani::proof]
fn claim_credential_operation_is_total_and_terminal_dominates() {
    // Cause/effect decision table: C1 terminal repair, C2 cancellation request.
    // R1 C1=* -> TerminalResolution; R2 !C1+C2 -> Cancel;
    // R3 !C1+!C2 -> Execute. The rules are disjoint and exhaustive, and R1
    // proves terminal repair cannot accidentally materialize credentials.
    let terminal_resolution: bool = kani::any();
    let cancellation_requested: bool = kani::any();
    let operation = claim_credential_operation(terminal_resolution, cancellation_requested);
    if terminal_resolution {
        assert!(matches!(
            operation,
            ClaimCredentialOperation::TerminalResolution
        ));
    } else if cancellation_requested {
        assert!(matches!(operation, ClaimCredentialOperation::Cancel));
    } else {
        assert!(matches!(operation, ClaimCredentialOperation::Execute));
    }
}

pub fn worker_credential_realization_capabilities(
    worker: &WorkerSnapshot,
) -> Result<CredentialRealizationCapabilities, AttemptCredentialBindingError> {
    CredentialRealizationCapabilities::from_manifest_capabilities(&worker.manifest.capabilities)
        .map_err(AttemptCredentialBindingError::InvalidWorkerCapabilities)
}

/// Whether one independently installed credential profile both declares the
/// exact ACP backend and admits its process-private MCP field. Recursing over
/// alternatives preserves adapter correlation: evidence from distinct profiles
/// can never be combined into a synthetic capability.
fn acp_mcp_client_injection_profile_admits(
    access: &awaken_runtime_contract::CredentialAccess,
    holder: &awaken_runtime_contract::PlaintextHolder,
    backend_ref: &str,
    installed: &CredentialRealizationCapabilities,
    now_unix_ms: u64,
) -> bool {
    let consumer_id = format!(
        "{}{}",
        awaken_runtime_contract::credential::ACP_CREDENTIAL_CONSUMER_PREFIX,
        backend_ref
    );
    (installed.extension_consumers.contains_key(&consumer_id)
        && access
            .admit(
                holder,
                awaken_runtime_contract::CredentialRealizationKind::ProcessProtocolField,
                installed,
                now_unix_ms,
            )
            .is_ok())
        || installed.alternatives.iter().any(|profile| {
            acp_mcp_client_injection_profile_admits(
                access,
                holder,
                backend_ref,
                profile,
                now_unix_ms,
            )
        })
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
                    let realization = match holder.boundary {
                        awaken_runtime_contract::PlaintextBoundary::Worker => {
                            awaken_runtime_contract::CredentialRealizationKind::WorkerRelay
                        }
                        awaken_runtime_contract::PlaintextBoundary::Workload => {
                            let execution_candidates = request
                                .activation
                                .snapshot
                                .resolved_spec
                                .execution_candidates(
                                    request.activation.model_ref_override.as_deref(),
                                );
                            if execution_candidates.is_empty()
                                || execution_candidates.iter().any(|candidate| {
                                    !matches!(
                                        awaken_runtime_contract::resolved::Backend::from_ref(
                                            &candidate.binding().backend_ref
                                        ),
                                        awaken_runtime_contract::resolved::Backend::Acp(_)
                                    )
                                })
                            {
                                return Err(
                                    DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder,
                                );
                            }
                            access
                                .admit(
                                    holder,
                                    awaken_runtime_contract::CredentialRealizationKind::ProcessProtocolField,
                                    installed,
                                    now_unix_ms,
                                )
                                .map_err(DispatchCredentialAdmissionError::SessionAdmission)?;
                            if execution_candidates.iter().any(|candidate| {
                                !acp_mcp_client_injection_profile_admits(
                                    access,
                                    holder,
                                    &candidate.binding().backend_ref,
                                    installed,
                                    now_unix_ms,
                                )
                            }) {
                                return Err(
                                    DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder,
                                );
                            }
                            continue;
                        }
                        awaken_runtime_contract::PlaintextBoundary::Platform => {
                            return Err(
                                DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder,
                            );
                        }
                    };
                    access
                        .admit(holder, realization, installed, now_unix_ms)
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

/// Compile the exact secret-free credential authority needed by one claim.
///
/// This is the sole operation classifier used by every dispatch store. A
/// cancellation intentionally excludes provider fallbacks, advisors, Session
/// MCP attachments, and sandbox credentials: none of those can authenticate
/// the A2A `tasks/get` / `tasks/cancel` calls owned by the selected remote
/// executor. Anonymous and non-remote cancellations remain credential-free.
pub fn compile_claim_credential_bindings(
    request: &RunDispatch,
    installed: &CredentialRealizationCapabilities,
    claim_epoch: u64,
    now_unix_ms: u64,
    operation: ClaimCredentialOperation,
) -> Result<Vec<AttemptCredentialBinding>, DispatchCredentialAdmissionError> {
    match operation {
        ClaimCredentialOperation::Execute => {
            compile_attempt_credential_bindings(request, installed, claim_epoch, now_unix_ms)
        }
        ClaimCredentialOperation::Cancel => {
            let candidate = &request.activation.snapshot.resolved_spec.model_binding;
            if !matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(
                    &candidate.binding().backend_ref
                ),
                awaken_runtime_contract::resolved::Backend::Remote(_)
            ) {
                return Ok(Vec::new());
            }
            awaken_runtime_contract::compile_candidate_credential_bindings(
                &[candidate],
                request.inference_plaintext_holder.as_ref(),
                installed,
                claim_epoch,
                now_unix_ms,
            )
            .map_err(DispatchCredentialAdmissionError::Attempt)
        }
        ClaimCredentialOperation::TerminalResolution => Ok(Vec::new()),
    }
}

/// Recompute the operation-specific claim decision and reject any widened,
/// missing, reordered, or stale wire projection at the Worker boundary.
pub fn verify_claim_credential_bindings(
    request: &RunDispatch,
    installed: &CredentialRealizationCapabilities,
    claim_epoch: u64,
    now_unix_ms: u64,
    operation: ClaimCredentialOperation,
    claimed: &[AttemptCredentialBinding],
) -> Result<(), DispatchCredentialAdmissionError> {
    let expected =
        compile_claim_credential_bindings(request, installed, claim_epoch, now_unix_ms, operation)?;
    if claimed != expected {
        return Err(DispatchCredentialAdmissionError::ClaimedBindingMismatch);
    }
    Ok(())
}

/// Recompute the one canonical claim decision and require the persisted wire
/// projection to match it exactly. This is the Worker-side trust-boundary check:
/// it neither searches for a holder nor repairs a stale binding.
pub fn verify_attempt_credential_bindings(
    request: &RunDispatch,
    installed: &CredentialRealizationCapabilities,
    claim_epoch: u64,
    now_unix_ms: u64,
    claimed: &[AttemptCredentialBinding],
) -> Result<(), DispatchCredentialAdmissionError> {
    verify_claim_credential_bindings(
        request,
        installed,
        claim_epoch,
        now_unix_ms,
        ClaimCredentialOperation::Execute,
        claimed,
    )
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
    can_admit_claim_credentials(
        request,
        installed,
        claim_epoch,
        now_unix_ms,
        ClaimCredentialOperation::Execute,
    )
}

/// Read-only eligibility for the same operation-specific compiler used by the
/// exact claim transaction. It never substitutes anonymous transport authority.
#[must_use]
pub fn can_admit_claim_credentials(
    request: &RunDispatch,
    installed: &CredentialRealizationCapabilities,
    claim_epoch: u64,
    now_unix_ms: u64,
    operation: ClaimCredentialOperation,
) -> bool {
    compile_claim_credential_bindings(request, installed, claim_epoch, now_unix_ms, operation)
        .is_ok()
}
