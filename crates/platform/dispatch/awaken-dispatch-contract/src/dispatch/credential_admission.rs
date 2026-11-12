// @formal entrypoint credential.mcp_claim_admits_only_when_every_requirement_is_installed
// @formal entrypoint credential_effect.boundary_preserves_exact_materialization_authority
use awaken_credential_contract::CredentialAccessAdmission;
use awaken_credential_contract::CredentialRealizationCapabilities;
use awaken_credential_contract::{AttemptCredentialBinding, AttemptCredentialBindingError};
use awaken_credential_contract::{
    McpCredentialRequirement, McpCredentialRequirementError, McpCredentialUsageClass,
};
use awaken_worker_contract::WorkerSnapshot;

use crate::run_dispatch::RunDispatch;

/// Complete claim-time credential admission failure for one dispatch.
///
/// Inference failures retain the Runtime contract's neutral vocabulary. Session
/// MCP failures belong here because durable Run ingress is the only boundary
/// that joins the Session's published MCP credential requirements to Worker
/// claim admission. `InvalidSessionCredentialProjection` and
/// `InvalidSessionMcpCredentialBinding` arise only from rows written before the
/// Session published those requirements.
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
    SessionAdmission(awaken_credential_contract::CredentialAdmissionError),
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
    access: &awaken_agent_contract::credential::CredentialAccess,
    holder: &awaken_agent_contract::credential::PlaintextHolder,
    backend_ref: &str,
    installed: &CredentialRealizationCapabilities,
    now_unix_ms: u64,
) -> bool {
    let consumer_id = format!(
        "{}{}",
        awaken_credential_contract::ACP_CREDENTIAL_CONSUMER_PREFIX,
        backend_ref
    );
    (installed.extension_consumers.contains_key(&consumer_id)
        && access
            .admit_for_preparation(
                holder,
                awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField,
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

/// The only realization route one typed Session MCP credential requirement
/// may take on the claiming Worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpRequirementRoute {
    RejectUsage,
    WorkerRelay,
    ClientInjection,
    RejectHolder,
}

/// Select the route from the requirement's usage class and selected holder.
/// The Session validated the usage at admission; it is classified again here
/// because the requirement is read back from a durable row, and an unsupported
/// usage must still fail closed. A Platform holder has no MCP route.
const fn mcp_requirement_route(
    usage: McpCredentialUsageClass,
    boundary: awaken_agent_contract::credential::PlaintextBoundary,
) -> McpRequirementRoute {
    match (usage, boundary) {
        (McpCredentialUsageClass::Unsupported, _) => McpRequirementRoute::RejectUsage,
        (
            McpCredentialUsageClass::AuthorizationBearer,
            awaken_agent_contract::credential::PlaintextBoundary::Worker,
        ) => McpRequirementRoute::WorkerRelay,
        (
            McpCredentialUsageClass::AuthorizationBearer,
            awaken_agent_contract::credential::PlaintextBoundary::Workload,
        ) => McpRequirementRoute::ClientInjection,
        (
            McpCredentialUsageClass::AuthorizationBearer,
            awaken_agent_contract::credential::PlaintextBoundary::Platform,
        ) => McpRequirementRoute::RejectHolder,
    }
}

/// A claim admits the Session MCP credentials only when every requirement is
/// admitted. The first refusal is the claim's refusal; no requirement can be
/// skipped, and an empty set admits.
fn every_mcp_requirement_admitted<E>(
    verdicts: impl IntoIterator<Item = Result<(), E>>,
) -> Result<(), E> {
    for verdict in verdicts {
        verdict?;
    }
    Ok(())
}

impl From<McpCredentialRequirementError> for DispatchCredentialAdmissionError {
    fn from(error: McpCredentialRequirementError) -> Self {
        match error {
            McpCredentialRequirementError::UnpairedHolder => {
                Self::InvalidSessionMcpCredentialBinding
            }
            McpCredentialRequirementError::UnsupportedUsage => {
                Self::InvalidSessionMcpCredentialUsage
            }
        }
    }
}

impl From<crate::run_dispatch::SessionMcpRequirementProjectionError>
    for DispatchCredentialAdmissionError
{
    fn from(error: crate::run_dispatch::SessionMcpRequirementProjectionError) -> Self {
        match error {
            crate::run_dispatch::SessionMcpRequirementProjectionError::Decode(message) => {
                Self::InvalidSessionCredentialProjection(message)
            }
            crate::run_dispatch::SessionMcpRequirementProjectionError::Requirement(error) => {
                error.into()
            }
        }
    }
}

/// Admit one frozen dependency for preparation against installed Worker
/// evidence. Its source expiry is not an effect grant: canonical Session
/// realization and the materializer must strictly admit the actual pin.
/// A Worker
/// holder is the legacy relay; a Workload holder is client injection, which
/// every effective execution backend must be an ACP adapter able to inject the
/// exact process-private field from one installed profile.
fn admit_session_mcp_requirement(
    request: &RunDispatch,
    requirement: &McpCredentialRequirement,
    installed: &CredentialRealizationCapabilities,
    now_unix_ms: u64,
) -> Result<(), DispatchCredentialAdmissionError> {
    let access = &requirement.credential;
    let holder = &requirement.selected_plaintext_holder;
    match mcp_requirement_route(requirement.usage_class(), holder.boundary) {
        McpRequirementRoute::RejectUsage => {
            Err(DispatchCredentialAdmissionError::InvalidSessionMcpCredentialUsage)
        }
        McpRequirementRoute::RejectHolder => {
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder)
        }
        McpRequirementRoute::WorkerRelay => access
            .admit_for_preparation(
                holder,
                awaken_credential_contract::CredentialRealizationKind::WorkerRelay,
                installed,
                now_unix_ms,
            )
            .map(|_plan| ())
            .map_err(DispatchCredentialAdmissionError::SessionAdmission),
        McpRequirementRoute::ClientInjection => {
            let execution_candidates = request
                .activation
                .snapshot
                .resolved_spec
                .execution_candidates(request.activation.model_ref_override.as_deref());
            if execution_candidates.is_empty()
                || execution_candidates.iter().any(|candidate| {
                    !matches!(
                        awaken_runtime_contract::resolved::Backend::from_ref(
                            candidate.binding().identity().2
                        ),
                        awaken_runtime_contract::resolved::Backend::Acp(_)
                    )
                })
            {
                return Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder);
            }
            access
                .admit_for_preparation(
                    holder,
                    awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField,
                    installed,
                    now_unix_ms,
                )
                .map_err(DispatchCredentialAdmissionError::SessionAdmission)?;
            if execution_candidates.iter().any(|candidate| {
                !acp_mcp_client_injection_profile_admits(
                    access,
                    holder,
                    candidate.binding().identity().2,
                    installed,
                    now_unix_ms,
                )
            }) {
                return Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder);
            }
            Ok(())
        }
    }
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
    let bindings = awaken_credential_contract::compile_candidate_credential_bindings(
        &candidates,
        request.inference_plaintext_holder.as_ref(),
        installed,
        claim_epoch,
        now_unix_ms,
    )?;
    let requirements = request.session_mcp_credential_requirements()?;
    every_mcp_requirement_admitted(requirements.iter().map(|requirement| {
        admit_session_mcp_requirement(request, requirement, installed, now_unix_ms)
    }))?;
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
                    candidate.binding().identity().2
                ),
                awaken_runtime_contract::resolved::Backend::Remote(_)
            ) {
                return Ok(Vec::new());
            }
            awaken_credential_contract::compile_candidate_credential_bindings(
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
    if !claim_credential_bindings_are_exact(&expected, claimed) {
        return Err(DispatchCredentialAdmissionError::ClaimedBindingMismatch);
    }
    Ok(())
}

/// Ordered, complete equality of the compiler result and persisted claim wire.
/// No caller may reinterpret the set, ignore order, or accept a subset.
#[must_use]
fn claim_credential_bindings_are_exact(
    expected: &[AttemptCredentialBinding],
    claimed: &[AttemptCredentialBinding],
) -> bool {
    expected == claimed
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

#[cfg(test)]
mod exhaustive {
    use super::*;
    use awaken_agent_contract::credential::PlaintextBoundary;

    const fn listed_usage(usage: McpCredentialUsageClass) -> usize {
        match usage {
            McpCredentialUsageClass::AuthorizationBearer => 0,
            McpCredentialUsageClass::Unsupported => 1,
        }
    }
    const EVERY_USAGE: [McpCredentialUsageClass; 2] = [
        McpCredentialUsageClass::AuthorizationBearer,
        McpCredentialUsageClass::Unsupported,
    ];

    const fn listed_boundary(boundary: PlaintextBoundary) -> usize {
        match boundary {
            PlaintextBoundary::Worker => 0,
            PlaintextBoundary::Workload => 1,
            PlaintextBoundary::Platform => 2,
        }
    }
    const EVERY_BOUNDARY: [PlaintextBoundary; 3] = [
        PlaintextBoundary::Worker,
        PlaintextBoundary::Workload,
        PlaintextBoundary::Platform,
    ];

    #[test]
    fn every_list_names_each_variant_once_in_match_order() {
        for (index, value) in EVERY_USAGE.into_iter().enumerate() {
            assert_eq!(listed_usage(value), index);
        }
        for (index, value) in EVERY_BOUNDARY.into_iter().enumerate() {
            assert_eq!(listed_boundary(value), index);
        }
    }

    /// @exhaustive bool(2) x bool(2) = 4
    /// @obligation credential_effect.boundary_preserves_exact_materialization_authority
    #[test]
    fn claim_credential_operation_is_total_and_terminal_dominates() {
        // Cause/effect decision table: C1 terminal repair, C2 cancellation request.
        // R1 C1=* -> TerminalResolution; R2 !C1+C2 -> Cancel;
        // R3 !C1+!C2 -> Execute. The rules are disjoint and exhaustive, and R1
        // shows terminal repair cannot accidentally materialize credentials.
        for terminal_resolution in [false, true] {
            for cancellation_requested in [false, true] {
                let operation =
                    claim_credential_operation(terminal_resolution, cancellation_requested);
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
        }
    }

    /// @exhaustive McpCredentialUsageClass(2) x PlaintextBoundary(3) = 6
    /// @obligation credential.mcp_claim_admits_only_when_every_requirement_is_installed
    #[test]
    fn mcp_requirement_route_admits_only_bearer_relay_or_workload_injection() {
        // Cause/effect table: C1 usage class is Authorization Bearer or not; C2
        // the selected holder boundary is Worker, Workload or Platform.
        // R1 !C1 -> RejectUsage for every holder; R2 C1+Worker -> WorkerRelay;
        // R3 C1+Workload -> ClientInjection; R4 C1+Platform -> RejectHolder. No
        // route admits a requirement without first selecting exactly one holder
        // mechanism that the installed evidence must then satisfy.
        for usage in EVERY_USAGE {
            for boundary in EVERY_BOUNDARY {
                let route = mcp_requirement_route(usage, boundary);
                match usage {
                    McpCredentialUsageClass::Unsupported => {
                        assert_eq!(route, McpRequirementRoute::RejectUsage, "R1");
                    }
                    McpCredentialUsageClass::AuthorizationBearer => match boundary {
                        PlaintextBoundary::Worker => {
                            assert_eq!(route, McpRequirementRoute::WorkerRelay, "R2");
                        }
                        PlaintextBoundary::Workload => {
                            assert_eq!(route, McpRequirementRoute::ClientInjection, "R3");
                        }
                        PlaintextBoundary::Platform => {
                            assert_eq!(route, McpRequirementRoute::RejectHolder, "R4");
                        }
                    },
                }
            }
        }
    }

    /// @exhaustive [bool; 3](8) x count 0..=3(4) = 32
    /// @obligation credential.mcp_claim_admits_only_when_every_requirement_is_installed
    #[test]
    fn claim_admits_session_mcp_credentials_only_when_every_requirement_is_admitted() {
        // Cause/effect table over up to three requirements: C1 how many there
        // are; C2 which of them the installed evidence admits. E1 every admitted
        // (including none present) -> the claim admits; E2 any refusal -> the
        // claim is refused with the first refusal, in requirement order. No
        // refused requirement can be skipped or outvoted by later admissions.
        for mask in 0_u8..8 {
            let admitted: [bool; 3] = std::array::from_fn(|bit| mask & (1 << bit) != 0);
            for count in 0..=3_usize {
                let verdict = every_mcp_requirement_admitted(
                    admitted[..count]
                        .iter()
                        .enumerate()
                        .map(|(index, ok)| if *ok { Ok(()) } else { Err(index) }),
                );
                let first_refusal = (0..count).find(|index| !admitted[*index]);
                match first_refusal {
                    None => assert_eq!(verdict, Ok(()), "E1"),
                    Some(index) => assert_eq!(verdict, Err(index), "E2"),
                }
            }
        }
    }

    /// @exhaustive case(5) = 5
    /// @obligation credential_effect.boundary_preserves_exact_materialization_authority
    #[test]
    fn claim_credential_binding_revalidation_rejects_missing_widened_reordered_or_stale_wire() {
        // Causes: C1 exact ordered pair; C2 missing entry; C3 extra entry;
        // C4 reordered pair; C5 one stale coordinate. Effects: E1 only C1 admits;
        // E2 C2-C5 reject before materialization. The receipt test independently
        // covers every field coordinate; this test owns collection completeness
        // and order using the production slice comparator.
        let binding = |identity: &str, epoch| AttemptCredentialBinding {
            candidate_fingerprint: awaken_agent_contract::credential::CandidateFingerprint(
                identity.into(),
            ),
            credential: awaken_agent_contract::credential::CredentialRef {
                id: identity.into(),
                revision: 1,
            },
            selected_plaintext_holder: awaken_agent_contract::credential::PlaintextHolder::new(
                PlaintextBoundary::Worker,
                identity,
            ),
            selected_realization_kind:
                awaken_credential_contract::CredentialRealizationKind::WorkerProviderAdapter,
            claim_epoch: epoch,
        };
        let first = binding("first", 7);
        let second = binding("second", 7);
        let expected = vec![first.clone(), second.clone()];
        for case in 0_u8..5 {
            let claimed = match case {
                0 => expected.clone(),
                1 => vec![first.clone()],
                2 => vec![first.clone(), second.clone(), second.clone()],
                3 => vec![second.clone(), first.clone()],
                _ => vec![binding("first", 8), binding("second", 7)],
            };
            assert_eq!(
                claim_credential_bindings_are_exact(&expected, &claimed),
                case == 0
            );
        }
    }
}
