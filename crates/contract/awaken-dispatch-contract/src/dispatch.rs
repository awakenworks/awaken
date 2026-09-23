//! The durable-ingress store ports and their data.
//!
//! Two aggregates, per the run-ingress DDD split:
//!
//! - [`DispatchQueue`] owns *delivery opportunity* — enqueue, single-owner claim,
//!   lease, settle, and lease-expiry recovery. It never owns message payload
//!   truth or a run's outcome; those are the thread and run aggregates, read back
//!   from committed facts.
//! - [`Inbox`] owns the *thread's pending input* — idempotent append of
//!   delivered-but-unconsumed input, frozen once at a safe run boundary.
//!
//! One concrete store implements both (so a wake can freeze pending input inside
//! the same claim transaction), but the traits stay split so neither aggregate
//! reaches into the other's invariants. A blanket [`Dispatch`] bundles them
//! for the worker and host.

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_runtime_contract::CredentialRealizationCapabilities;
pub use awaken_runtime_contract::{
    AttemptCredentialBinding, AttemptCredentialBindingError, AttemptCredentialDelivery,
    CandidateFingerprint, CredentialRealizationPersistenceDecision, CredentialRealizationReceipt,
    CredentialReceiptError, credential_realization_persistence_decision,
    verify_credential_realization_receipt,
};
#[cfg(test)]
use awaken_runtime_contract::{
    CredentialAdmissionError, CredentialRealizationKind, CredentialUsage, PlaintextBoundary,
};
use awaken_session_execution_contract::{
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
};
use awaken_worker_contract::{PlacementPolicy, WorkerAssignment, WorkerIdentity, WorkerSnapshot};
use serde::{Deserialize, Serialize};

use crate::run_dispatch::RunDispatch;

include!("commit_epoch.rs");

include!("dispatch/delivery.rs");

include!("dispatch/pending_settlement.rs");

mod credential_admission;
pub use credential_admission::{
    ClaimCredentialOperation, DispatchCredentialAdmissionError, can_admit_attempt_credentials,
    can_admit_claim_credentials, claim_credential_operation, compile_attempt_credential_bindings,
    compile_claim_credential_bindings, verify_attempt_credential_bindings,
    verify_claim_credential_bindings, worker_credential_realization_capabilities,
};

/// Admission or durable-store failure classified by what the caller may safely
/// conclude. `Rejected` proves that this command did not commit, `Conflict`
/// proves another immutable identity already exists, and `Unavailable`
/// deliberately proves neither: retry must preserve the exact dispatch identity.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DispatchError {
    #[error("dispatch store rejected: {0}")]
    Rejected(String),
    #[error("dispatch identity conflict: {0}")]
    Conflict(String),
    #[error("dispatch store unavailable: {0}")]
    Unavailable(String),
}

impl DispatchError {
    pub fn unavailable(error: impl std::fmt::Display) -> Self {
        Self::Unavailable(error.to_string())
    }
}

include!("dispatch/settlement.rs");

mod attempt_binding;
pub use attempt_binding::{
    AttemptExecutionBinding, PhysicalAttempt, attempt_binding_retry_is_exact,
};

mod model;
pub use model::*;

include!("dispatch/queue.rs");

mod aggregate;
pub use aggregate::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_replacement_safety_partition_is_exhaustive() {
        // Cause/effect graph: each closed DispatchState either has crossed the
        // settled Awaiting boundary/already been superseded, or may still own
        // execution/admission activity. Effect: only the first partition is
        // replaceable. This table must name every variant so adding a phase
        // forces an explicit policy decision.
        let cases = [
            (DispatchState::Reserved, false),
            (DispatchState::ReservationLeased, false),
            (DispatchState::Pending, false),
            (DispatchState::Leased, false),
            (DispatchState::Awaiting, true),
            (DispatchState::DeadLetter, false),
            (DispatchState::Superseded, true),
        ];
        for (state, expected) in cases {
            assert_eq!(
                session_run_replacement_candidate_is_safe(state),
                expected,
                "closed replacement policy for {state:?}"
            );
        }
    }
    use awaken_runtime_contract::{
        CredentialAccess, CredentialEnvelope, CredentialExecutionPolicy, CredentialMaterialSource,
        CredentialRef, CredentialRefreshAccess, InferenceEndpoint, ModelExposurePolicy,
        PlaintextHolder, SealedCredentialEnvelopeRef, TokenEndpointAuth, TrustDomainRef,
    };
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };

    pub(super) fn a_request() -> RunDispatch {
        let activation = RunActivation::new(
            RunId("run-1".into()),
            ThreadId("thrd-1".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snap".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("agent".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: "be helpful".into(),
                    max_steps: Some(8),
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("prov", "model", "acp:test"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            vec![Message::text(MessageId("u1".into()), Role::User, "go")],
        );
        RunDispatch::new(activation)
    }

    fn holder(boundary: PlaintextBoundary, domain: &str) -> PlaintextHolder {
        PlaintextHolder::new(boundary, domain)
    }

    fn provider_candidate(
        provider: &str,
        model: &str,
        backend: &str,
        credential_id: &str,
        allowed_holder: &PlaintextHolder,
    ) -> awaken_runtime_contract::resolved::ResolvedModelCandidate {
        candidate_with_access(
            provider,
            model,
            backend,
            CredentialAccess::new(
                CredentialRef {
                    id: credential_id.into(),
                    revision: 7,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::ProviderAdapter,
                CredentialExecutionPolicy::exact(
                    allowed_holder.clone(),
                    ModelExposurePolicy::Forbidden,
                ),
            )
            .with_target(awaken_runtime_contract::CredentialTarget::new(
                awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                provider,
            )),
        )
    }

    fn candidate_with_access(
        provider: &str,
        model: &str,
        backend: &str,
        access: CredentialAccess,
    ) -> awaken_runtime_contract::resolved::ResolvedModelCandidate {
        let binding = ModelBinding::new(
            provider,
            if backend.starts_with("a2a:") {
                ""
            } else {
                model
            },
            backend,
        );
        if backend.starts_with("a2a:") {
            return awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
                binding,
                "workspace-a",
                Some(access),
                "sha256:test-agent-card",
            )
            .expect("coherent dispatch remote candidate");
        }
        let endpoint = InferenceEndpoint {
            adapter_kind: "openai".into(),
            api_dialect: "open_ai_chat".into(),
            base_url: "https://provider.invalid/v1".into(),
            upstream_model: model.into(),
            processing_placement: None,
        };
        let result = if backend.starts_with("acp:") {
            awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider_with_acp(
                binding,
                format!("{provider}@1"),
                format!("{provider}-route@1"),
                "workspace-a",
                Some(access),
                endpoint,
                awaken_runtime_contract::resolved::AcpExecutionProfile {
                    supports_tool_aliases: false,
                    capability_adapter_version: "test".into(),
                    capability_fingerprint: "sha256:test-capability".into(),
                    session_configuration: Default::default(),
                },
            )
        } else {
            awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
                binding,
                format!("{provider}@1"),
                format!("{provider}-route@1"),
                "workspace-a",
                Some(access),
                endpoint,
            )
        };
        result.expect("coherent dispatch provider candidate")
    }

    fn request_with_candidates(
        primary: awaken_runtime_contract::resolved::ResolvedModelCandidate,
        fallbacks: Vec<awaken_runtime_contract::resolved::ResolvedModelCandidate>,
        selected_holder: Option<PlaintextHolder>,
    ) -> RunDispatch {
        let mut request = a_request();
        request.activation.snapshot.resolved_spec.model_binding = primary;
        request.activation.snapshot.resolved_spec.model_candidates = fallbacks;
        request.inference_plaintext_holder = selected_holder;
        request
    }

    fn capabilities(
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
    ) -> CredentialRealizationCapabilities {
        CredentialRealizationCapabilities {
            holders: BTreeSet::from([selected_holder.clone()]),
            material_sources: BTreeSet::from([CredentialMaterialSource::ControlPlaneReference]),
            realization_kinds: BTreeSet::from([realization]),
            recipient_bound_envelopes: true,
            envelope_required: false,
            extension_consumers: Default::default(),
            alternatives: Vec::new(),
        }
    }

    fn acp_mcp_capabilities(
        selected_holder: &PlaintextHolder,
        backend_ref: &str,
    ) -> CredentialRealizationCapabilities {
        let mut installed = capabilities(
            selected_holder,
            CredentialRealizationKind::ProcessProtocolField,
        );
        installed.extension_consumers.insert(
            format!(
                "{}{}",
                awaken_runtime_contract::credential::ACP_CREDENTIAL_CONSUMER_PREFIX,
                backend_ref
            ),
            BTreeSet::from(["awaken.credential.mcp-process-protocol-field/v1".to_string()]),
        );
        installed
    }

    fn session_runtime_with_mcp(
        credential: Option<CredentialAccess>,
        selected_plaintext_holder: Option<PlaintextHolder>,
    ) -> crate::SessionRuntimeEnvelope {
        let worker = holder(PlaintextBoundary::Worker, "worker-a");
        let mcp_holder = selected_plaintext_holder
            .clone()
            .unwrap_or_else(|| worker.clone());
        crate::SessionRuntimeEnvelope::from_projection(
            awaken_session_configuration_contract::EnvironmentSnapshot {
                environment_id: "environment-a".into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: true,
                config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                    "environment-a@1".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                    inference_holder: worker.clone(),
                    mcp_holder,
                    resource_holder: worker,
                },
            },
            Some(Default::default()),
            vec![awaken_session_mcp_contract::StageMcpAttachment {
                workspace_id: "workspace-a".into(),
                generation: awaken_session_mcp_contract::McpGenerationRef {
                    session_id: "session-a".into(),
                    generation: awaken_session_mcp_contract::McpGeneration::of_bare_target(
                        awaken_session_mcp_contract::McpAttachmentId("mcp-a".into()),
                        1,
                        &awaken_session_mcp_contract::McpTarget::parse_http(
                            "https://mcp.example.test",
                        )
                        .expect("valid MCP target"),
                    ),
                    runtime_incarnation: "worker-a".into(),
                    lease_epoch: 1,
                    lease_expires_at_unix_ms: 10_000,
                },
                realization_id: "realization-a".into(),
                stage_idempotency_key: "stage-a".into(),
                name: "mcp-a".into(),
                target: awaken_session_mcp_contract::McpTarget::parse_http(
                    "https://mcp.example.test",
                )
                .expect("valid MCP target"),
                prompts_as_skills: false,
                credential,
                selected_plaintext_holder,
            }],
        )
        .expect("Session runtime projection serializes")
    }

    #[test]
    fn session_mcp_credentials_join_the_existing_claim_admission() {
        // Cause/effect graph: C1 credential/holder are absent or paired; C2
        // usage is canonical Authorization Bearer; C3 holder is legacy Worker,
        // ACP Workload, or unsupported Platform; C4 every effective backend is
        // ACP for Workload delivery; C5 one installed profile contains the exact
        // holder/source/ProcessProtocolField plus that backend's declaration.
        // Effects: E1 credential-free, legacy relay, and exact client injection
        // admit without adding an MCP attempt binding; E2 malformed/unsupported
        // requests fail before Worker effects; E3 capability failures retain the
        // common admission error. Constraint: alternative profiles never combine
        // a backend declaration from one adapter with mechanism evidence from
        // another; the Session generation remains holder/receipt authority.
        //
        // | Rule | binding | usage | holder/backend | installed evidence | Effect |
        // | M1 | none | - | - | - | E1 credential-free |
        // | M2 | paired | bearer | Worker | exact relay | E1 legacy relay |
        // | M3 | paired | bearer | Workload/all ACP | same-profile exact marker | E1 client injection |
        // | M4 | unpaired | - | - | - | E2 invalid binding |
        // | M5 | paired | other | Worker | exact relay | E2 invalid usage |
        // | M6 | paired | bearer | Platform | otherwise exact | E2 unsupported holder |
        // | M7 | paired | bearer | Workload/non-ACP | otherwise exact | E2 unsupported backend |
        // | M8 | paired | bearer | Workload/ACP | missing mechanism | E3 holder/mechanism unsupported |
        // | M9 | paired | bearer | Workload/ACP | missing source | E3 source unsupported |
        // | M10 | paired | bearer | Workload/ACP | marker names another backend | E2 unsupported adapter |
        // | M11 | paired | bearer | Workload/ACP | marker/mechanism split across profiles | E2 no cross-profile synthesis |
        let worker = holder(PlaintextBoundary::Worker, "worker-a");
        let workload = holder(PlaintextBoundary::Workload, "workload-a");
        let platform = holder(PlaintextBoundary::Platform, "platform-a");
        let access = |selected: &PlaintextHolder, usage| {
            CredentialAccess::new(
                CredentialRef {
                    id: "mcp-credential".into(),
                    revision: 3,
                },
                CredentialMaterialSource::ControlPlaneReference,
                usage,
                CredentialExecutionPolicy::exact(selected.clone(), ModelExposurePolicy::Forbidden),
            )
            .with_target(awaken_runtime_contract::CredentialTarget::new(
                awaken_runtime_contract::credential::CredentialPurpose::McpAuthorization,
                "https://mcp.example.test/",
            ))
        };
        let worker_installed = capabilities(&worker, CredentialRealizationKind::WorkerRelay);

        let mut anonymous = a_request();
        anonymous.session_runtime = Some(session_runtime_with_mcp(None, None));
        assert_eq!(
            compile_attempt_credential_bindings(&anonymous, &worker_installed, 1, 10),
            Ok(Vec::new()),
            "M1"
        );

        let worker_bearer = access(
            &worker,
            CredentialUsage::HttpHeader {
                name: "Authorization".into(),
                scheme: Some("Bearer".into()),
            },
        );
        let mut legacy_relay = a_request();
        legacy_relay.session_runtime = Some(session_runtime_with_mcp(
            Some(worker_bearer.clone()),
            Some(worker.clone()),
        ));
        assert_eq!(
            compile_attempt_credential_bindings(&legacy_relay, &worker_installed, 1, 10),
            Ok(Vec::new()),
            "M2"
        );

        let workload_bearer = access(
            &workload,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("bearer".into()),
            },
        );
        let workload_installed = acp_mcp_capabilities(&workload, "acp:test");
        let mut client_injection = a_request();
        client_injection.session_runtime = Some(session_runtime_with_mcp(
            Some(workload_bearer.clone()),
            Some(workload.clone()),
        ));
        assert_eq!(
            compile_attempt_credential_bindings(&client_injection, &workload_installed, 1, 10),
            Ok(Vec::new()),
            "M3"
        );

        let mut unpaired = a_request();
        unpaired.session_runtime =
            Some(session_runtime_with_mcp(Some(worker_bearer.clone()), None));
        assert_eq!(
            compile_attempt_credential_bindings(&unpaired, &worker_installed, 1, 10),
            Err(DispatchCredentialAdmissionError::InvalidSessionMcpCredentialBinding),
            "M4"
        );

        let mut invalid_usage = a_request();
        invalid_usage.session_runtime = Some(session_runtime_with_mcp(
            Some(access(&worker, CredentialUsage::ProviderAdapter)),
            Some(worker.clone()),
        ));
        assert_eq!(
            compile_attempt_credential_bindings(&invalid_usage, &worker_installed, 1, 10),
            Err(DispatchCredentialAdmissionError::InvalidSessionMcpCredentialUsage),
            "M5"
        );

        let mut platform_request = a_request();
        platform_request.session_runtime = Some(session_runtime_with_mcp(
            Some(access(
                &platform,
                CredentialUsage::HttpHeader {
                    name: "Authorization".into(),
                    scheme: Some("Bearer".into()),
                },
            )),
            Some(platform.clone()),
        ));
        assert_eq!(
            compile_attempt_credential_bindings(
                &platform_request,
                &capabilities(&platform, CredentialRealizationKind::PlatformRelay),
                1,
                10,
            ),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
            "M6"
        );

        let mut non_acp = client_injection.clone();
        non_acp.activation.snapshot.resolved_spec.model_binding =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
                "prov", "model", "genai",
            ));
        assert_eq!(
            compile_attempt_credential_bindings(&non_acp, &workload_installed, 1, 10),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
            "M7"
        );

        let mut missing_kind = workload_installed.clone();
        missing_kind.realization_kinds.clear();
        assert_eq!(
            compile_attempt_credential_bindings(&client_injection, &missing_kind, 1, 10),
            Err(DispatchCredentialAdmissionError::SessionAdmission(
                CredentialAdmissionError::HolderUnsupported,
            )),
            "M8"
        );

        let mut missing_source = workload_installed.clone();
        missing_source.material_sources.clear();
        assert_eq!(
            compile_attempt_credential_bindings(&client_injection, &missing_source, 1, 10),
            Err(DispatchCredentialAdmissionError::SessionAdmission(
                CredentialAdmissionError::MaterialSourceUnsupported,
            )),
            "M9"
        );

        let mismatched_adapter = acp_mcp_capabilities(&workload, "acp:other");
        assert_eq!(
            compile_attempt_credential_bindings(&client_injection, &mismatched_adapter, 1, 10),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
            "M10"
        );

        let mechanism_only =
            capabilities(&workload, CredentialRealizationKind::ProcessProtocolField);
        let mut marker_only = CredentialRealizationCapabilities::default();
        marker_only.extension_consumers.insert(
            format!(
                "{}{}",
                awaken_runtime_contract::credential::ACP_CREDENTIAL_CONSUMER_PREFIX,
                "acp:test"
            ),
            BTreeSet::from(["awaken.credential.mcp-process-protocol-field/v1".to_string()]),
        );
        let split_profiles =
            CredentialRealizationCapabilities::alternatives([mechanism_only, marker_only]);
        assert_eq!(
            compile_attempt_credential_bindings(&client_injection, &split_profiles, 1, 10),
            Err(DispatchCredentialAdmissionError::UnsupportedSessionMcpHolder),
            "M11"
        );
    }

    #[derive(Clone, Copy)]
    enum BindingFixture {
        CredentialFree,
        MissingHolder,
        InvalidUsage,
        NativeWorker,
        NativePlatform,
        AcpWorkload,
        AcpWorker,
        NativeWorkload,
        RemoteWorker,
        HolderUnsupported,
        SourceUnsupported,
        HolderForbidden,
        EnvelopeExpired,
        RefreshRevisionMismatch,
        DuplicateCandidate,
    }

    #[derive(Clone)]
    enum BindingExpected {
        Empty,
        One(CredentialRealizationKind),
        Error(AttemptCredentialBindingError),
    }

    struct BindingRule {
        id: &'static str,
        fixture: BindingFixture,
        claim_epoch: u64,
        expected: BindingExpected,
    }

    /// Cause-effect graph for one selected publication candidate:
    ///
    /// C0 claim epoch > 0
    ///   -> C1 selected candidate carries credential
    ///      -> C2 exact holder requested -> C3 ProviderAdapter usage
    ///      -> C4 backend/holder cell implemented
    ///      -> C5 installed capability + published policy admit the exact tuple
    ///      -> C6 candidate content identity is unique -> E1 frozen binding.
    ///
    /// Credential-free candidates yield E0 (no binding). Every failed cause is
    /// terminal and yields its stable error; the credential contract's own
    /// decision table exhaustively covers the internals of C5.
    /// Envelope liveness belongs to execution, not immutable claim identity:
    /// B14 admits the binding but refuses direct use of the expired carrier.
    /// The HTTP redelivery test owns fresh delivery or exact claim relinquish.
    ///
    /// | Rule | C0 | C1 | C2 | C3 | C4 | C5 | C6 | Result |
    /// |---|---|---|---|---|---|---|---|---|
    /// | B1 | F | - | - | - | - | - | - | invalid epoch |
    /// | B2 | T | F | - | - | - | - | - | empty |
    /// | B3 | T | T | F | - | - | - | - | missing holder |
    /// | B4 | T | T | T | F | - | - | - | invalid usage |
    /// | B5 | T | T | T | T | Native/Worker | T | T | provider adapter |
    /// | B6 | T | T | T | T | Native/Platform | T | T | platform adapter |
    /// | B7 | T | T | T | T | ACP/Workload | T | T | process secret env |
    /// | B8 | T | T | T | T | ACP/Worker | T | T | worker relay |
    /// | B9 | T | T | T | T | Native/Workload | - | - | unsupported |
    /// | B10 | T | T | T | T | Remote/Worker | T | T | worker relay |
    /// | B11 | T | T | T | T | valid | F(holder) | - | unsupported holder |
    /// | B12 | T | T | T | T | valid | F(source) | - | unsupported source |
    /// | B13 | T | T | T | T | valid | F(policy) | - | forbidden holder |
    /// | B14 | T | T | T | T | valid | T | T | binding; expired carrier unusable |
    /// | B15 | T | T | T | T | valid | F(revision) | - | refresh mismatch |
    /// | B16 | T | T | T | T | valid | T | F | duplicate candidate |
    #[test]
    fn attempt_binding_cases_are_generated_from_the_decision_table() {
        let rules = [
            BindingRule {
                id: "B1",
                fixture: BindingFixture::NativeWorker,
                claim_epoch: 0,
                expected: BindingExpected::Error(AttemptCredentialBindingError::InvalidClaimEpoch),
            },
            BindingRule {
                id: "B2",
                fixture: BindingFixture::CredentialFree,
                claim_epoch: 3,
                expected: BindingExpected::Empty,
            },
            BindingRule {
                id: "B3",
                fixture: BindingFixture::MissingHolder,
                claim_epoch: 3,
                expected: BindingExpected::Error(
                    AttemptCredentialBindingError::MissingPlaintextHolder,
                ),
            },
            BindingRule {
                id: "B4",
                fixture: BindingFixture::InvalidUsage,
                claim_epoch: 3,
                expected: BindingExpected::Error(
                    AttemptCredentialBindingError::InvalidCredentialUsage,
                ),
            },
            BindingRule {
                id: "B5",
                fixture: BindingFixture::NativeWorker,
                claim_epoch: 3,
                expected: BindingExpected::One(CredentialRealizationKind::WorkerProviderAdapter),
            },
            BindingRule {
                id: "B6",
                fixture: BindingFixture::NativePlatform,
                claim_epoch: 3,
                expected: BindingExpected::One(CredentialRealizationKind::PlatformProviderAdapter),
            },
            BindingRule {
                id: "B7",
                fixture: BindingFixture::AcpWorkload,
                claim_epoch: 3,
                expected: BindingExpected::One(CredentialRealizationKind::ProcessSecretEnvironment),
            },
            BindingRule {
                id: "B8",
                fixture: BindingFixture::AcpWorker,
                claim_epoch: 3,
                expected: BindingExpected::One(CredentialRealizationKind::WorkerRelay),
            },
            BindingRule {
                id: "B9",
                fixture: BindingFixture::NativeWorkload,
                claim_epoch: 3,
                expected: BindingExpected::Error(
                    AttemptCredentialBindingError::UnsupportedRealization {
                        boundary: PlaintextBoundary::Workload,
                        backend: "genai".into(),
                    },
                ),
            },
            BindingRule {
                id: "B10",
                fixture: BindingFixture::RemoteWorker,
                claim_epoch: 3,
                expected: BindingExpected::One(CredentialRealizationKind::WorkerRelay),
            },
            BindingRule {
                id: "B11",
                fixture: BindingFixture::HolderUnsupported,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::Admission(
                    CredentialAdmissionError::HolderUnsupported,
                )),
            },
            BindingRule {
                id: "B12",
                fixture: BindingFixture::SourceUnsupported,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::Admission(
                    CredentialAdmissionError::MaterialSourceUnsupported,
                )),
            },
            BindingRule {
                id: "B13",
                fixture: BindingFixture::HolderForbidden,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::Admission(
                    CredentialAdmissionError::HolderNotAllowed,
                )),
            },
            BindingRule {
                id: "B14",
                fixture: BindingFixture::EnvelopeExpired,
                claim_epoch: 3,
                expected: BindingExpected::One(CredentialRealizationKind::WorkerProviderAdapter),
            },
            BindingRule {
                id: "B15",
                fixture: BindingFixture::RefreshRevisionMismatch,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::Admission(
                    CredentialAdmissionError::CredentialRevisionMismatch,
                )),
            },
            BindingRule {
                id: "B16",
                fixture: BindingFixture::DuplicateCandidate,
                claim_epoch: 3,
                expected: BindingExpected::Error(AttemptCredentialBindingError::DuplicateCandidate),
            },
        ];

        for rule in rules {
            let worker_holder = holder(PlaintextBoundary::Worker, "worker-a");
            let workload_holder = holder(PlaintextBoundary::Workload, "workload-a");
            let platform_holder = holder(PlaintextBoundary::Platform, "platform-a");
            let (backend, selected_holder, realization) = match rule.fixture {
                BindingFixture::NativePlatform => (
                    "genai",
                    platform_holder.clone(),
                    CredentialRealizationKind::PlatformProviderAdapter,
                ),
                BindingFixture::AcpWorkload | BindingFixture::NativeWorkload => (
                    if matches!(rule.fixture, BindingFixture::AcpWorkload) {
                        "acp:claude"
                    } else {
                        "genai"
                    },
                    workload_holder.clone(),
                    CredentialRealizationKind::ProcessSecretEnvironment,
                ),
                BindingFixture::AcpWorker => (
                    "acp:claude",
                    worker_holder.clone(),
                    CredentialRealizationKind::WorkerRelay,
                ),
                BindingFixture::RemoteWorker => (
                    "a2a:https://agent.invalid",
                    worker_holder.clone(),
                    CredentialRealizationKind::WorkerRelay,
                ),
                _ => (
                    "genai",
                    worker_holder.clone(),
                    CredentialRealizationKind::WorkerProviderAdapter,
                ),
            };
            let usage = if matches!(
                rule.fixture,
                BindingFixture::InvalidUsage | BindingFixture::RemoteWorker
            ) {
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                }
            } else {
                CredentialUsage::ProviderAdapter
            };
            let policy = if matches!(rule.fixture, BindingFixture::HolderForbidden) {
                CredentialExecutionPolicy::exact(
                    holder(PlaintextBoundary::Worker, "worker-b"),
                    ModelExposurePolicy::Forbidden,
                )
            } else {
                CredentialExecutionPolicy::exact(
                    selected_holder.clone(),
                    ModelExposurePolicy::Forbidden,
                )
            };
            let mut access = CredentialAccess::new(
                CredentialRef {
                    id: "credential-a".into(),
                    revision: 7,
                },
                CredentialMaterialSource::ControlPlaneReference,
                usage,
                policy,
            )
            .with_target(if matches!(rule.fixture, BindingFixture::RemoteWorker) {
                awaken_runtime_contract::CredentialTarget::new(
                    awaken_runtime_contract::credential::CredentialPurpose::RemoteAgentAuthorization,
                    "https://agent.invalid",
                )
            } else {
                awaken_runtime_contract::CredentialTarget::new(
                    awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                    "provider-a",
                )
            });
            if matches!(rule.fixture, BindingFixture::EnvelopeExpired) {
                access = access.with_envelope(CredentialEnvelope::SealedForWorker {
                    envelope_ref: SealedCredentialEnvelopeRef {
                        id: "envelope-a".into(),
                        payload_fingerprint: "sha256:payload".into(),
                    },
                    recipient: TrustDomainRef(selected_holder.trust_domain.0.clone()),
                    expires_at_unix_ms: 9,
                });
                assert_eq!(
                    access.admit(
                        &selected_holder,
                        realization,
                        &capabilities(&selected_holder, realization),
                        10,
                    ),
                    Err(CredentialAdmissionError::EnvelopeExpired),
                    "B14: claim admission must not make the expired carrier executable",
                );
            }
            if matches!(rule.fixture, BindingFixture::RefreshRevisionMismatch) {
                access = access.with_refresh(CredentialRefreshAccess::new(
                    8,
                    "https://auth.invalid/token".into(),
                    "client-a".into(),
                    TokenEndpointAuth::None,
                    None,
                    "refresh-a".into(),
                    "access-a".into(),
                    None,
                    None,
                ));
            }
            let candidate = candidate_with_access("provider-a", "model-a", backend, access);
            let primary = if matches!(rule.fixture, BindingFixture::CredentialFree) {
                awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
                    "host", "model-a", "native",
                ))
            } else {
                candidate.clone()
            };
            let fallbacks = if matches!(rule.fixture, BindingFixture::DuplicateCandidate) {
                vec![candidate]
            } else {
                Vec::new()
            };
            let requested_holder = if matches!(
                rule.fixture,
                BindingFixture::CredentialFree | BindingFixture::MissingHolder
            ) {
                None
            } else {
                Some(selected_holder.clone())
            };
            let request = request_with_candidates(primary, fallbacks, requested_holder);
            let mut installed = capabilities(&selected_holder, realization);
            // C5 for an ACP Workload covers the last-mile mechanism as well: the
            // installed evidence must advertise the exact delivery extension for
            // this backend, or no realization exists to admit.
            if matches!(rule.fixture, BindingFixture::AcpWorkload) {
                installed.extension_consumers.insert(
                    format!(
                        "{}{backend}",
                        awaken_runtime_contract::credential::ACP_CREDENTIAL_CONSUMER_PREFIX
                    ),
                    BTreeSet::from([
                        awaken_runtime_contract::credential::PROCESS_SECRET_ENVIRONMENT_MATERIAL_TYPE
                            .to_string(),
                    ]),
                );
            }
            if matches!(rule.fixture, BindingFixture::HolderUnsupported) {
                installed.holders.clear();
            }
            if matches!(rule.fixture, BindingFixture::SourceUnsupported) {
                installed.material_sources.clear();
            }
            let actual =
                compile_attempt_credential_bindings(&request, &installed, rule.claim_epoch, 10);
            match rule.expected {
                BindingExpected::Empty => assert_eq!(actual, Ok(Vec::new()), "{}", rule.id),
                BindingExpected::One(kind) => {
                    let bindings = actual.unwrap_or_else(|error| panic!("{}: {error}", rule.id));
                    assert_eq!(bindings.len(), 1, "{}", rule.id);
                    assert_eq!(bindings[0].selected_realization_kind, kind, "{}", rule.id);
                    assert_eq!(bindings[0].claim_epoch, rule.claim_epoch, "{}", rule.id);
                    assert_eq!(bindings[0].credential.id, "credential-a", "{}", rule.id);
                    assert!(
                        bindings[0].candidate_fingerprint.0.starts_with("sha256:"),
                        "{}",
                        rule.id
                    );
                }
                BindingExpected::Error(expected) => {
                    assert_eq!(
                        actual,
                        Err(DispatchCredentialAdmissionError::Attempt(expected)),
                        "{}",
                        rule.id
                    );
                }
            }
        }
    }

    #[path = "credential_admission_tests.rs"]
    mod credential_admission_tests;

    /// Candidate-selection cause graph:
    ///
    /// C1 nonblank override -> select every published route for that model.
    /// !C1 -> retain the complete ordered primary/fallback pool.
    /// Credential-free selected candidates contribute no binding, while every
    /// credential-bearing candidate contributes exactly one distinct binding.
    ///
    /// | Rule | override | selected publication | Result |
    /// |---|---|---|---|
    /// | S1 | absent | primary + both fallbacks | three ordered bindings |
    /// | S2 | model-b | both model-b routes only | two ordered bindings |
    /// | S3 | model-c | credential-free host only | empty |
    #[test]
    fn binding_set_preserves_published_fallbacks_and_model_override_scope() {
        let selected = holder(PlaintextBoundary::Worker, "worker-a");
        let primary =
            provider_candidate("provider-a", "model-a", "genai", "credential-a", &selected);
        let route_b1 = provider_candidate(
            "provider-b1",
            "model-b",
            "genai",
            "credential-b1",
            &selected,
        );
        let route_b2 = provider_candidate(
            "provider-b2",
            "model-b",
            "genai",
            "credential-b2",
            &selected,
        );
        let host = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            ModelBinding::new("host", "model-c", "native"),
        );
        let installed = capabilities(&selected, CredentialRealizationKind::WorkerProviderAdapter);
        let base = request_with_candidates(primary, vec![route_b1, route_b2, host], Some(selected));

        let all = compile_attempt_credential_bindings(&base, &installed, 11, 10)
            .expect("S1 complete pool binds");
        assert_eq!(
            all.iter()
                .map(|binding| binding.credential.id.as_str())
                .collect::<Vec<_>>(),
            vec!["credential-a", "credential-b1", "credential-b2"]
        );
        assert_eq!(
            all.iter()
                .map(|binding| &binding.candidate_fingerprint)
                .collect::<BTreeSet<_>>()
                .len(),
            3,
            "each publication route has a distinct content identity"
        );

        let mut model_b = base.clone();
        model_b.activation.model_ref_override = Some("model-b".into());
        let selected_b = compile_attempt_credential_bindings(&model_b, &installed, 12, 10)
            .expect("S2 same-model routes bind");
        assert_eq!(
            selected_b
                .iter()
                .map(|binding| binding.credential.id.as_str())
                .collect::<Vec<_>>(),
            vec!["credential-b1", "credential-b2"]
        );

        let mut model_c = base;
        model_c.activation.model_ref_override = Some("model-c".into());
        assert_eq!(
            compile_attempt_credential_bindings(&model_c, &installed, 13, 10),
            Ok(Vec::new()),
            "S3 selected host candidate needs no credential binding"
        );
    }

    /// Claimed-binding revalidation cause/effect graph: C1 immutable dispatch,
    /// C2 installed capabilities, C3 claim epoch, and C4 the complete ordered
    /// persisted bindings all match the canonical compiler. E1 only C1-C4
    /// admits execution; changing any binding coordinate, cardinality, or order
    /// produces E2 fail-closed. The compiler error remains E3 rather than being
    /// hidden as a wire mismatch.
    ///
    /// | Rule | C1/C2 | C3 | C4 | Effect |
    /// |---|---|---|---|---|
    /// | V1 | exact | exact | exact | E1 admit |
    /// | V2 | exact | changed | stale bindings | E2 reject |
    /// | V3 | exact | exact | field/cardinality/order changed | E2 reject |
    /// | V4 | compiler rejects | any | any | E3 preserve compiler error |
    #[test]
    fn claimed_binding_revalidation_requires_the_exact_compiled_claim_set() {
        let selected = holder(PlaintextBoundary::Worker, "worker-a");
        let primary =
            provider_candidate("provider-a", "model-a", "genai", "credential-a", &selected);
        let fallback =
            provider_candidate("provider-b", "model-b", "genai", "credential-b", &selected);
        let request = request_with_candidates(primary, vec![fallback], Some(selected.clone()));
        let installed = capabilities(&selected, CredentialRealizationKind::WorkerProviderAdapter);
        let exact = compile_attempt_credential_bindings(&request, &installed, 7, 10)
            .expect("V1 exact claim compiles");
        assert_eq!(
            verify_attempt_credential_bindings(&request, &installed, 7, 10, &exact),
            Ok(()),
            "V1/E1"
        );

        let mut mutations = Vec::new();
        let mut changed = exact.clone();
        changed[0].claim_epoch = 8;
        mutations.push(("epoch", changed));
        let mut changed = exact.clone();
        changed[0].credential.revision += 1;
        mutations.push(("credential", changed));
        let mut changed = exact.clone();
        changed[0].selected_plaintext_holder =
            holder(PlaintextBoundary::Worker, "worker-substitution");
        mutations.push(("holder", changed));
        let mut changed = exact.clone();
        changed[0].selected_realization_kind = CredentialRealizationKind::WorkerRelay;
        mutations.push(("mechanism", changed));
        let mut changed = exact.clone();
        changed[0].candidate_fingerprint = CandidateFingerprint("sha256:substituted".into());
        mutations.push(("candidate", changed));
        let mut changed = exact.clone();
        changed.pop();
        mutations.push(("missing", changed));
        let mut changed = exact.clone();
        changed.push(exact[0].clone());
        mutations.push(("extra", changed));
        let mut changed = exact.clone();
        changed.swap(0, 1);
        mutations.push(("order", changed));
        for (case, changed) in mutations {
            assert_eq!(
                verify_attempt_credential_bindings(&request, &installed, 7, 10, &changed),
                Err(DispatchCredentialAdmissionError::ClaimedBindingMismatch),
                "V3/E2 {case}"
            );
        }
        assert_eq!(
            verify_attempt_credential_bindings(&request, &installed, 8, 10, &exact),
            Err(DispatchCredentialAdmissionError::ClaimedBindingMismatch),
            "V2/E2"
        );

        let mut unsupported = installed;
        unsupported.holders.clear();
        assert!(
            matches!(
                verify_attempt_credential_bindings(&request, &unsupported, 7, 10, &exact),
                Err(DispatchCredentialAdmissionError::Attempt(
                    AttemptCredentialBindingError::Admission(_)
                ))
            ),
            "V4/E3"
        );
    }

    /// Receipt cause-effect graph:
    ///
    /// exact binding identity + exact planned/actual mechanism + intact content
    /// fingerprint -> verified receipt. Any mismatch fails closed before the
    /// receipt can become durable evidence.
    ///
    /// | Rule | binding fields | mechanism | fingerprint | Result |
    /// |---|---|---|---|---|
    /// | P1 | exact | exact | exact | verified |
    /// | P2 | mismatch | exact | exact | binding mismatch |
    /// | P3 | exact | mismatch | - | mechanism mismatch |
    /// | P4 | exact | exact | mismatch | fingerprint mismatch |
    #[test]
    fn realization_receipt_cases_are_generated_from_the_decision_table() {
        let binding = AttemptCredentialBinding {
            candidate_fingerprint: CandidateFingerprint("sha256:candidate-a".into()),
            credential: CredentialRef {
                id: "credential-a".into(),
                revision: 7,
            },
            selected_plaintext_holder: holder(PlaintextBoundary::Worker, "worker-a"),
            selected_realization_kind: CredentialRealizationKind::WorkerProviderAdapter,
            claim_epoch: 9,
        };
        let valid = CredentialRealizationReceipt::new(
            &binding,
            CredentialRealizationKind::WorkerProviderAdapter,
        )
        .expect("P1 exact receipt");
        assert_eq!(valid.verify(&binding), Ok(()));

        let mismatched_bindings = [
            AttemptCredentialBinding {
                candidate_fingerprint: CandidateFingerprint("sha256:candidate-b".into()),
                ..binding.clone()
            },
            AttemptCredentialBinding {
                credential: CredentialRef {
                    id: "credential-b".into(),
                    revision: 7,
                },
                ..binding.clone()
            },
            AttemptCredentialBinding {
                selected_plaintext_holder: holder(PlaintextBoundary::Worker, "worker-b"),
                ..binding.clone()
            },
            AttemptCredentialBinding {
                claim_epoch: 10,
                ..binding.clone()
            },
        ];
        for mismatched in mismatched_bindings {
            assert_eq!(
                valid.verify(&mismatched),
                Err(CredentialReceiptError::BindingMismatch),
                "P2 every binding coordinate is fenced"
            );
        }

        let mut wrong_mechanism = valid.clone();
        wrong_mechanism.actual_realization_kind = CredentialRealizationKind::WorkerRelay;
        assert_eq!(
            wrong_mechanism.verify(&binding),
            Err(CredentialReceiptError::MechanismMismatch),
            "P3 actual mechanism cannot differ from admission"
        );

        let mut tampered = valid.clone();
        tampered.receipt_fingerprint = "sha256:tampered".into();
        assert_eq!(
            tampered.verify(&binding),
            Err(CredentialReceiptError::FingerprintMismatch),
            "P4 receipt content is tamper-evident"
        );

        let wire = serde_json::to_string(&(binding, valid)).expect("receipt wire serializes");
        assert!(!wire.contains("secret-material"));
        assert!(!wire.contains("api_key"));
    }

    /// The SQL backends' `status` column maps to the public enum, and any value
    /// outside the known set is rejected rather than becoming runnable.
    #[test]
    fn dispatch_status_from_db_maps_known_values_and_rejects_unknowns() {
        // Test design. Causes: C1 each persisted status uses the exact canonical
        // spelling; C2 the value is case-variant, unknown, or empty. Effects: E1
        // C1 maps to its public state; E2 C2 is rejected. Constraint/Invariant:
        // unrecognized durable vocabulary must never become runnable. Decision rule:
        // cover every supported state plus one representative from each
        // invalid class (case mismatch, unknown, and empty).
        assert_eq!(
            DispatchState::from_db("reserved"),
            Some(DispatchState::Reserved)
        );
        assert_eq!(
            DispatchState::from_db("reservation_running"),
            Some(DispatchState::ReservationLeased)
        );
        assert_eq!(
            DispatchState::from_db("running"),
            Some(DispatchState::Leased)
        );
        assert_eq!(
            DispatchState::from_db("dead_letter"),
            Some(DispatchState::DeadLetter)
        );
        assert_eq!(
            DispatchState::from_db("superseded"),
            Some(DispatchState::Superseded)
        );
        assert_eq!(
            DispatchState::from_db("pending"),
            Some(DispatchState::Pending)
        );
        assert_eq!(DispatchState::from_db("Running"), None); // case-sensitive
        assert_eq!(DispatchState::from_db("bogus"), None);
        assert_eq!(DispatchState::from_db(""), None);
    }

    /// The neutral submit default every existing caller inherits: ordinary priority,
    /// no dedupe key, no supersede — so `enqueue` is behavior-unchanged.
    #[test]
    fn submit_options_default_is_ordinary_priority_no_dedupe_no_supersede() {
        let o = SubmitOptions::default();
        assert_eq!(o.priority, 0);
        assert!(o.dedupe_key.is_none());
        assert!(!o.supersede);
    }

    /// The claim/settle wire payloads round-trip through JSON — the lease a worker
    /// holds and the outcome it settles with must survive the HTTP dispatch
    /// transport (a cross-node worker claims/settles over the wire, not the DB).
    #[test]
    fn lease_and_outcome_round_trip_through_serde() {
        let lease = Lease {
            run_id: RunId("run-1".into()),
            owner: "host-7-42".into(),
            expires_ms: 9_999,
            epoch: 3,
        };
        let back: Lease = serde_json::from_str(&serde_json::to_string(&lease).expect("serializes"))
            .expect("deserializes");
        assert_eq!(back, lease);

        // A pre-fence lease row (no `epoch` key) loads with epoch 0 — the neutral
        // value a never-claimed row carries, so an old persisted/wire lease is not
        // rejected (the fence only fires when a HIGHER epoch supersedes it).
        let mut v = serde_json::to_value(&lease).expect("to value");
        v.as_object_mut().expect("object").remove("epoch");
        let legacy: Lease = serde_json::from_value(v).expect("legacy lease loads");
        assert_eq!(legacy.epoch, 0);

        for outcome in [DispatchOutcome::Done, DispatchOutcome::Awaiting] {
            let back: DispatchOutcome =
                serde_json::from_str(&serde_json::to_string(&outcome).expect("serializes"))
                    .expect("deserializes");
            assert_eq!(back, outcome);
        }
    }

    /// `applied()` is the single predicate the worker branches on after a settle: it
    /// is true only for `Applied`, so a `Fenced` (stale-owner) settle never reads as
    /// success and the stale owner abandons the run.
    #[test]
    fn settle_outcome_applied_is_true_only_when_applied() {
        assert!(SettleOutcome::Applied.applied());
        assert!(!SettleOutcome::Fenced.applied());
    }

    /// `SettleOutcome` is a settle *response*; a cross-node worker settles over the
    /// wire, so both variants must survive a JSON round-trip intact.
    #[test]
    fn settle_outcome_round_trips_through_serde() {
        for o in [SettleOutcome::Applied, SettleOutcome::Fenced] {
            let back: SettleOutcome =
                serde_json::from_str(&serde_json::to_string(&o).expect("serializes"))
                    .expect("deserializes");
            assert_eq!(back, o);
        }
    }

    /// The full claim payload — request + lease + pending + a bound sandbox ref —
    /// round-trips through JSON, since a cross-node worker receives `Claimed` over the
    /// dispatch transport, not out of the DB.
    #[test]
    fn claimed_round_trips_through_serde_including_sandbox_binding() {
        // Test design. Causes: C1 a claimed payload carries a bound sandbox and
        // cancellation/recovery flags; C2 the same payload is unplaced. Effects:
        // E1 C1 round-trips every authority-bearing field; E2 C2 preserves
        // sandbox=None. Constraint/Invariant: transport serialization must not
        // alter claim ownership or synthesize placement. Decision rule: exercise
        // the bound and unbound sandbox partitions over the same claim.
        let claimed = Claimed {
            request: a_request(),
            lease: Lease {
                run_id: RunId("run-1".into()),
                owner: "host-7".into(),
                expires_ms: 5_000,
                epoch: 2,
            },
            session_work_authority: None,
            credential_bindings: Vec::new(),
            credential_deliveries: Vec::new(),
            cancellation_requested: true,
            pending: vec![pending()],
            recovered: true,
            session_activity_admission_required: false,
            sandbox: Some("sbx-opaque-ref".into()),
            assignment: None,
        };
        let back: Claimed =
            serde_json::from_str(&serde_json::to_string(&claimed).expect("serializes"))
                .expect("deserializes");
        assert_eq!(back, claimed);
        assert_eq!(back.sandbox.as_deref(), Some("sbx-opaque-ref"));
        assert!(back.cancellation_requested);

        // An unplaced run (no sandbox yet) round-trips with `sandbox: None`.
        let unplaced = Claimed {
            sandbox: None,
            ..claimed
        };
        let back: Claimed =
            serde_json::from_str(&serde_json::to_string(&unplaced).expect("serializes"))
                .expect("deserializes");
        assert_eq!(back.sandbox, None);
    }

    /// A `DispatchQueue` that records the options its `enqueue_with` was called with,
    /// to prove the default `enqueue` delegates at `SubmitOptions::default()` and that
    /// `bind_sandbox` defaults to a no-op `Ok(())`. Every other method fails with an
    /// explicit typed error, so extending the test cannot introduce a panic path.
    #[derive(Default)]
    struct CapturingQueue {
        last_options: Mutex<Option<SubmitOptions>>,
    }

    impl CapturingQueue {
        fn unsupported<T>() -> Result<T, DispatchError> {
            Err(DispatchError::Rejected(
                "operation is outside the CapturingQueue test scope".to_string(),
            ))
        }
    }

    #[async_trait]
    impl DispatchQueue for CapturingQueue {
        async fn resolve_claimed_session_run_reservation(
            &self,
            _claim: &RunClaim,
            _resolution: SessionRunReservationResolution,
        ) -> Result<SettleOutcome, DispatchError> {
            Self::unsupported()
        }

        async fn worker_owns_run(
            &self,
            _identity: &WorkerIdentity,
            _run_id: &RunId,
        ) -> Result<Option<RunClaim>, DispatchError> {
            Self::unsupported()
        }

        async fn record_credential_realization(
            &self,
            _claim: &RunClaim,
            _receipt: CredentialRealizationReceipt,
        ) -> Result<SettleOutcome, DispatchError> {
            Self::unsupported()
        }

        async fn lock_commit_epoch(
            &self,
            _claim: &RunClaim,
        ) -> Result<Option<CommitEpochGuard>, DispatchError> {
            Self::unsupported()
        }

        async fn enqueue_with(
            &self,
            _request: RunDispatch,
            options: SubmitOptions,
        ) -> Result<(), DispatchError> {
            *self.last_options.lock().unwrap() = Some(options);
            Ok(())
        }
        async fn claim_new_run(
            &self,
            _request: RunDispatch,
            _owner: &str,
            _lease_ms: u64,
            _capabilities: &CredentialRealizationCapabilities,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }
        async fn claim_new_run_compatible(
            &self,
            _request: RunDispatch,
            _worker: &WorkerSnapshot,
            _lease_ms: u64,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }
        async fn deliver_and_claim(
            &self,
            _input: PendingInput,
            _owner: &str,
            _lease_ms: u64,
            _capabilities: &CredentialRealizationCapabilities,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }
        async fn deliver_and_claim_compatible(
            &self,
            _input: PendingInput,
            _worker: &WorkerSnapshot,
            _lease_ms: u64,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }
        async fn claim(
            &self,
            _owner: &str,
            _lease_ms: u64,
            _capabilities: &CredentialRealizationCapabilities,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }
        async fn claim_compatible(
            &self,
            _worker: &WorkerSnapshot,
            _lease_ms: u64,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }

        async fn claim_retry_exhausted(
            &self,
            _owner: &str,
            _lease_ms: u64,
            _max_attempts: u64,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }

        async fn claim_run(
            &self,
            _run_id: &RunId,
            _owner: &str,
            _lease_ms: u64,
            _capabilities: &CredentialRealizationCapabilities,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }
        async fn claim_run_compatible(
            &self,
            _run_id: &RunId,
            _worker: &WorkerSnapshot,
            _lease_ms: u64,
        ) -> Result<Option<Claimed>, DispatchError> {
            Self::unsupported()
        }

        async fn begin_attempt(
            &self,
            _claim: &RunClaim,
            _binding: Option<&AttemptExecutionBinding>,
        ) -> Result<AttemptAdmission, DispatchError> {
            Self::unsupported()
        }

        async fn handoff_attempt(
            &self,
            _claim: &RunClaim,
            _binding: Option<&AttemptExecutionBinding>,
        ) -> Result<AttemptAdmission, DispatchError> {
            Self::unsupported()
        }

        async fn finish_attempt(&self, _claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
            Self::unsupported()
        }

        async fn relinquish_claim(
            &self,
            _claim: &RunClaim,
        ) -> Result<SettleOutcome, DispatchError> {
            Self::unsupported()
        }

        async fn defer_claim(
            &self,
            _claim: &RunClaim,
            _retry_after_ms: u64,
        ) -> Result<SettleOutcome, DispatchError> {
            Self::unsupported()
        }
        async fn renew_lease(
            &self,
            _claim: &RunClaim,
            _lease_ms: u64,
        ) -> Result<LeaseRenewalOutcome, DispatchError> {
            Self::unsupported()
        }
        async fn claim_is_current(&self, _claim: &RunClaim) -> Result<bool, DispatchError> {
            Self::unsupported()
        }
        async fn settle(
            &self,
            _run_id: &RunId,
            _epoch: u64,
            _outcome: DispatchOutcome,
            _consumed: &[String],
        ) -> Result<SettleOutcome, DispatchError> {
            Self::unsupported()
        }
        async fn quarantine_retry_exhausted(&self, _: u64, _: u64) -> Result<usize, DispatchError> {
            Self::unsupported()
        }
        async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
            Self::unsupported()
        }
        async fn requeue(&self, _run_id: &RunId) -> Result<bool, DispatchError> {
            Self::unsupported()
        }
        async fn cancel(&self, _run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
            Self::unsupported()
        }
        async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
            Self::unsupported()
        }
        async fn purge_dead_letters_before(&self, _cutoff_ms: u64) -> Result<usize, DispatchError> {
            Self::unsupported()
        }
        async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
            Self::unsupported()
        }
        async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
            Self::unsupported()
        }
    }

    #[test]
    fn worker_side_queue_has_no_control_rung() {
        // Cause/effect rules: L1 a queue that does not own the Control-side
        // rung answers `control()` with None, so bounded identity reads,
        // reservations and placement claims are unreachable from a Worker
        // handle at the type level instead of failing at the effect; L2 a
        // queue that owns the rung answers Some(self) and the Control suite
        // (`awaken-dispatch-testkit`) checks its bounded reads never fall
        // back to a full scan.
        let q = CapturingQueue::default();
        assert!(
            q.control().is_none(),
            "L1 no Control rung on a capturing queue"
        );
    }

    #[tokio::test]
    async fn enqueue_delegates_to_enqueue_with_at_default_options() {
        let q = CapturingQueue::default();
        q.enqueue(a_request()).await.expect("enqueue ok");
        assert_eq!(
            q.last_options.lock().unwrap().clone(),
            Some(SubmitOptions::default()),
            "the convenience enqueue must submit at default priority/dedupe/supersede"
        );
    }

    #[tokio::test]
    async fn bind_sandbox_defaults_to_fail_closed() {
        // Backends that do not persist the binding cannot pretend the write applied.
        let q = CapturingQueue::default();
        assert_eq!(
            q.bind_sandbox(
                &RunClaim {
                    run_id: RunId("run-1".into()),
                    owner: "worker".into(),
                    epoch: 1,
                },
                "sbx-ref"
            )
            .await
            .unwrap(),
            SettleOutcome::Fenced
        );
    }
}
