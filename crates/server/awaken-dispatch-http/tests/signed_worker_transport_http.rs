// @formal evidence run_ingress.request_local_resolution_retry_honors_persisted_deadline
// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
// @formal evidence session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
// @formal evidence worker.accepted_protocol_version_is_inside_advertised_range
// awaken-allow(file-limit-ceiling): 3223 (legacy oversized test file: split, never grow)
//! The production request identity is wired through lifecycle and dispatch
//! clients over a real socket, including the post-registration incarnation bind.

use awaken_dispatch_testkit::worker_http::current_dispatch_codec_manifest;
use awaken_session_types::SessionId;
#[path = "signed_worker_transport_http/attempt_binding.rs"]
mod attempt_binding;
#[path = "signed_worker_transport_http/cursor_protocol.rs"]
mod cursor_protocol;
#[path = "signed_worker_transport_http/error_classes.rs"]
mod error_classes;
#[path = "signed_worker_transport_http/registry_incarnation.rs"]
mod registry_incarnation;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::Observation as StreamObservation;
use awaken_coordinator_client::{WorkerControlClient, WorkerControlSessionClient};
use awaken_dispatch::{
    AttemptAdmission, DispatchEnqueue as _, DispatchReads as _, MemoryDispatchStore,
    RegisteredWorker, RegistryError, RegistryMutation, RunDispatch, SettleOutcome, WorkerDirectory,
    WorkerHeartbeat, WorkerIdentity, WorkerManifest, WorkerRegistration, WorkerSnapshot,
    WorkerState,
};
use awaken_dispatch_http::{
    WorkerDispatchService, dispatch_transport_router_with_service, worker_environment_warmup_router,
};
use awaken_lease_testkit::ManualClock;
use awaken_runtime_contract::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryStreamSink;
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, SignedWorkerAuthenticator, SignedWorkerRequestAuthorizer,
    WorkerRequestAuthenticator, WorkerSigningCredential, WorkerUpstream,
};

#[derive(Default)]
struct RecordingSessionControl {
    clock: Option<Arc<ManualClock>>,
    projection: Mutex<Option<awaken_session_execution_contract::FrozenSessionProjection>>,
    activations: Mutex<usize>,
    acknowledgements: Mutex<usize>,
    failures: Mutex<usize>,
    begins: Mutex<Vec<awaken_session_execution_contract::BeginSessionRealization>>,
    renewals: Mutex<Vec<awaken_session_execution_contract::RenewSessionRealization>>,
    renewal_failure:
        Mutex<Option<awaken_session_execution_contract::SessionRealizationControlFailure>>,
    agent_lists: AtomicUsize,
    agent_messages: Mutex<Vec<awaken_session_execution_contract::SessionAgentMessageCommand>>,
    agent_boundaries: Mutex<Vec<awaken_session_execution_contract::SessionAgentBoundaryCommand>>,
    terminal_control_calls: AtomicUsize,
    cleanup_claims: Mutex<Vec<awaken_session_execution_contract::SessionRealizationTarget>>,
    cleanup_after: Mutex<Vec<Option<awaken_session_types::SessionId>>>,
    cleanup_action: Mutex<Option<awaken_session_contract::SessionTerminalCleanupAction>>,
    preparation_authorization: Mutex<
        Option<awaken_session_execution_contract::SessionTerminalCleanupPreparationAuthorization>,
    >,
    authorized_preparations: Mutex<Vec<awaken_session_contract::SessionTerminalCleanupEffect>>,
    cleanup_preparations: Mutex<Vec<awaken_session_contract::SessionCleanupPreparation>>,
    authorized_disposals: Mutex<Vec<awaken_session_contract::SessionTerminalCleanupDisposalEffect>>,
    cleanup_disposals: Mutex<Vec<awaken_session_contract::SessionCleanupDisposalReceipt>>,
    repository_publication_projection:
        Mutex<Option<awaken_session_execution_contract::SessionRepositoryPublicationProjection>>,
    repository_publication_receipts:
        Mutex<Vec<awaken_session_contract::SessionRepositoryPublicationReceipt>>,
    repository_publication_rejections:
        Mutex<Vec<awaken_session_contract::SessionRepositoryPublicationRejection>>,
}

impl RecordingSessionControl {
    fn receipt(
        &self,
        lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationLeaseReceipt,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        use awaken_time::WallClockExt as _;
        // The same injected authority clock drives HTTP admission and response
        // publication. Fixtures rejected before Control need no clock at all.
        awaken_session_execution_contract::SessionRealizationLeaseReceipt::issued_at(
            lease,
            self.clock
                .as_ref()
                .expect("Control reached with its clock")
                .now_ms(),
        )
    }
}

#[derive(Clone, Default)]
enum EnvironmentAuthorizationMode {
    #[default]
    Authorized,
    AlreadyApplied(String),
    Unowned,
    Unavailable,
}

#[derive(Default)]
struct RecordingEnvironmentBindings {
    mode: Mutex<EnvironmentAuthorizationMode>,
    intents: Mutex<Vec<awaken_session_contract::SessionEnvironmentEffectIntent>>,
    receipts: Mutex<Vec<awaken_session_contract::SessionEnvironmentReceipt>>,
    persisted_environment: Mutex<awaken_session_contract::SessionEnvironmentState>,
    reject_persist: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionEnvironmentBindingSink
    for RecordingEnvironmentBindings
{
    async fn authorize(
        &self,
        intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_types::RunError,
    > {
        self.intents.lock().unwrap().push(intent.clone());
        match self.mode.lock().unwrap().clone() {
            EnvironmentAuthorizationMode::Authorized => Ok(
                awaken_session_contract::SessionEnvironmentEffectAuthorization::Authorized {
                    intent: Box::new(intent.clone()),
                },
            ),
            EnvironmentAuthorizationMode::AlreadyApplied(binding) => Ok(
                awaken_session_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                    binding,
                    intent: Box::new(intent.clone()),
                },
            ),
            EnvironmentAuthorizationMode::Unowned => Ok(
                awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned {
                    intent: Box::new(intent.clone()),
                },
            ),
            EnvironmentAuthorizationMode::Unavailable => {
                Err(awaken_session_types::RunError::unavailable_classified(
                    "session_environment_authorization_unavailable",
                    "binding authority is temporarily unavailable",
                ))
            }
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_types::RunError>
    {
        if self.reject_persist.load(Ordering::SeqCst) {
            return Err(awaken_session_types::RunError::unavailable_classified(
                "session_environment_persist_unavailable",
                "binding persistence is temporarily unavailable",
            ));
        }
        self.receipts.lock().unwrap().push(receipt);
        Ok(self.persisted_environment.lock().unwrap().clone())
    }
}

#[derive(Default)]
struct RecordingSessionWorkAuthority {
    owner: Mutex<Option<String>>,
    retired: std::sync::atomic::AtomicBool,
    acquisitions: AtomicUsize,
    releases: AtomicUsize,
    acquired_sessions: Mutex<Vec<String>>,
    released_sessions: Mutex<Vec<String>>,
    reconciliation_requests: AtomicUsize,
}

/// The Work store owns its lease deadline, granted on its own clock; this
/// selected observation carries a fixed one that no caller judges.
const OBSERVED_WORK_EXPIRY_MS: u64 = 60_000;

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerLeaseAuthority
    for RecordingSessionWorkAuthority
{
    fn request_lifecycle_reconciliation(&self) {
        self.reconciliation_requests.fetch_add(1, Ordering::SeqCst);
    }

    async fn acquire_session_work(
        &self,
        session_id: &SessionId,
        worker_owner: &str,
        _acquisition: awaken_session_application_contract::SessionWorkAcquisition,
    ) -> Result<
        awaken_session_application_contract::SessionWorkOwnership,
        awaken_session_types::RunError,
    > {
        self.acquisitions.fetch_add(1, Ordering::SeqCst);
        self.acquired_sessions
            .lock()
            .unwrap()
            .push(session_id.to_string());
        if self.retired.load(Ordering::SeqCst) {
            return Ok(awaken_session_application_contract::SessionWorkOwnership::Unowned);
        }
        let mut owner = self.owner.lock().unwrap();
        let leased = |owner: &str| {
            awaken_session_application_contract::SessionWorkOwnership::Leased(
                awaken_session_types::SessionWorkLease::from_persisted(
                    "work-session",
                    "env",
                    session_id.clone(),
                    owner,
                    1,
                    OBSERVED_WORK_EXPIRY_MS,
                )
                .map(awaken_session_application_contract::SessionWorkGrant::from_transfer)
                .expect("nonzero epoch"),
            )
        };
        if let Some(current) = owner.as_deref().filter(|current| *current != worker_owner) {
            return Ok(leased(current));
        }
        *owner = Some(worker_owner.to_string());
        Ok(leased(worker_owner))
    }

    async fn release_session_work(
        &self,
        lease: &awaken_session_application_contract::SessionWorkGrant,
    ) -> Result<bool, awaken_session_types::RunError> {
        self.released_sessions
            .lock()
            .unwrap()
            .push(lease.session_id().to_string());
        let mut owner = self.owner.lock().unwrap();
        if owner.as_deref() != Some(lease.owner()) {
            return Ok(false);
        }
        *owner = None;
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    async fn release_worker_session_work(
        &self,
        worker_owner: &str,
    ) -> Result<usize, awaken_session_types::RunError> {
        let mut owner = self.owner.lock().unwrap();
        if owner.as_deref() != Some(worker_owner) {
            return Ok(0);
        }
        *owner = None;
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(1)
    }
}

fn frozen_projection() -> awaken_session_execution_contract::FrozenSessionProjection {
    let holder = awaken_agent_contract::credential::PlaintextHolder::new(
        awaken_agent_contract::credential::PlaintextBoundary::Worker,
        "test.worker",
    );
    let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment: awaken_session_configuration_contract::EnvironmentSnapshot {
                environment_id: "env".into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: true,
                config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                    "env-fingerprint".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                credential_realization: awaken_credential_contract::CredentialRealizationProfile {
                    inference_holder: holder.clone(),
                    mcp_holder: holder.clone(),
                    resource_holder: holder,
                },
            },
            runtime_placement: awaken_session_types::SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model_override: None,
            model: "model".into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    awaken_session_execution_contract::FrozenSessionProjection {
        bound_source_end: None,
        workspace_id: "workspace".into(),
        revision: awaken_session_types::SessionRevision(2),
        baseline,
        agent_publication: None,
        environment_materialization: None,
        environment: Default::default(),
        resource_revision: 0,
        resources: Default::default(),
        previous_resource_manifest: None,
        mcp: Vec::new(),
        tools: Default::default(),
        request_context: Vec::new(),
    }
}
#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRealizationControl for RecordingSessionControl {
    async fn begin_session_realization(
        &self,
        command: awaken_session_execution_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationDirective,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.begins.lock().unwrap().push(command.clone());
        let projection =
            self.projection.lock().unwrap().clone().ok_or(
                awaken_session_execution_contract::SessionRealizationControlFailure::NotReady,
            )?;
        Ok(
            awaken_session_execution_contract::SessionRealizationDirective {
                projection,
                authority: self.receipt(
                    awaken_session_configuration_contract::resource::SessionRealizationLease {
                        owner: command.target.owner,
                        runtime_incarnation: command.target.runtime_incarnation,
                        epoch: 2,
                        expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
                    },
                )?,
                action: awaken_session_execution_contract::SessionRealizationAction::Complete,
            },
        )
    }

    async fn renew_session_realization(
        &self,
        command: awaken_session_execution_contract::RenewSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationLeaseReceipt,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.renewals.lock().unwrap().push(command.clone());
        if let Some(error) = self.renewal_failure.lock().unwrap().clone() {
            return Err(error);
        }
        let mut lease = command.asserted_lease;
        lease.expires_at_unix_ms = command.requested_expires_at_unix_ms;
        self.receipt(lease)
    }

    async fn activate_session_realization(
        &self,
        command: awaken_session_execution_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationDirective,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        *self.activations.lock().unwrap() += 1;
        let projection =
            self.projection.lock().unwrap().clone().ok_or(
                awaken_session_execution_contract::SessionRealizationControlFailure::NotReady,
            )?;
        Ok(
            awaken_session_execution_contract::SessionRealizationDirective {
                projection,
                authority: self.receipt(command.lease)?,
                action: awaken_session_execution_contract::SessionRealizationAction::Complete,
            },
        )
    }

    async fn acknowledge_session_realization(
        &self,
        command: awaken_session_execution_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationDirective,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        *self.acknowledgements.lock().unwrap() += 1;
        let projection =
            self.projection.lock().unwrap().clone().ok_or(
                awaken_session_execution_contract::SessionRealizationControlFailure::NotReady,
            )?;
        Ok(
            awaken_session_execution_contract::SessionRealizationDirective {
                projection,
                authority: self.receipt(command.lease)?,
                action: awaken_session_execution_contract::SessionRealizationAction::Complete,
            },
        )
    }

    async fn fail_session_realization(
        &self,
        _command: awaken_session_execution_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationControlFailure> {
        *self.failures.lock().unwrap() += 1;
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionTerminalCleanupQueue for RecordingSessionControl {
    async fn claim_next_terminal_cleanup(
        &self,
        target: awaken_session_execution_contract::SessionRealizationTarget,
        after_session_id: Option<&awaken_session_types::SessionId>,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionTerminalCleanupAssignment>,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.terminal_control_calls.fetch_add(1, Ordering::SeqCst);
        self.cleanup_claims.lock().unwrap().push(target.clone());
        self.cleanup_after
            .lock()
            .unwrap()
            .push(after_session_id.cloned());
        let projection =
            self.projection.lock().unwrap().clone().ok_or(
                awaken_session_execution_contract::SessionRealizationControlFailure::NotReady,
            )?;
        Ok(Some(
            awaken_session_execution_contract::SessionTerminalCleanupAssignment {
                environment_creation_not_attempted: false,
                session_id: "signed-terminal-recovery".into(),
                projection,
                authority: self.receipt(
                    awaken_session_configuration_contract::resource::SessionRealizationLease {
                        owner: target.owner,
                        runtime_incarnation: target.runtime_incarnation,
                        epoch: 9,
                        expires_at_unix_ms: target.lease_expires_at_unix_ms,
                    },
                )?,
            },
        ))
    }

    async fn terminal_cleanup_work(
        &self,
        session_id: &SessionId,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionTerminalCleanupWork>,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.terminal_control_calls.fetch_add(1, Ordering::SeqCst);
        let Some(action) = self.cleanup_action.lock().unwrap().clone() else {
            return Ok(None);
        };
        let projection =
            self.projection.lock().unwrap().clone().ok_or(
                awaken_session_execution_contract::SessionRealizationControlFailure::NotReady,
            )?;
        Ok(Some(
            awaken_session_execution_contract::SessionTerminalCleanupWork {
                assignment: awaken_session_execution_contract::SessionTerminalCleanupAssignment {
                    environment_creation_not_attempted: false,
                    session_id: session_id.clone(),
                    projection,
                    authority: self.receipt(lease.clone())?,
                },
                action,
            },
        ))
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionTerminalPreparationControl
    for RecordingSessionControl
{
    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_execution_contract::SessionTerminalCleanupPreparationAuthorization,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.terminal_control_calls.fetch_add(1, Ordering::SeqCst);
        self.authorized_preparations
            .lock()
            .unwrap()
            .push(effect.clone());
        self.preparation_authorization
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_execution_contract::SessionRealizationControlFailure::NotReady)
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        _lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        preparation: awaken_session_contract::SessionCleanupPreparation,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationControlFailure> {
        self.terminal_control_calls.fetch_add(1, Ordering::SeqCst);
        self.cleanup_preparations.lock().unwrap().push(preparation);
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionTerminalDisposalControl for RecordingSessionControl {
    async fn authorize_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, awaken_session_execution_contract::SessionRealizationControlFailure> {
        self.terminal_control_calls.fetch_add(1, Ordering::SeqCst);
        self.authorized_disposals
            .lock()
            .unwrap()
            .push(effect.clone());
        Ok("workspace".into())
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        _lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationControlFailure> {
        self.terminal_control_calls.fetch_add(1, Ordering::SeqCst);
        self.cleanup_disposals.lock().unwrap().push(receipt);
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionTerminalRepositoryPublicationControl
    for RecordingSessionControl
{
    async fn terminal_repository_publication_command(
        &self,
        _session_id: &SessionId,
        _lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionRepositoryPublicationProjection>,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.terminal_control_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .repository_publication_projection
            .lock()
            .unwrap()
            .clone())
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        _session_id: &SessionId,
        _lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationControlFailure> {
        self.terminal_control_calls.fetch_add(1, Ordering::SeqCst);
        self.repository_publication_receipts
            .lock()
            .unwrap()
            .push(receipt);
        Ok(())
    }

    async fn record_terminal_repository_publication_rejection(
        &self,
        _session_id: &SessionId,
        _lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationControlFailure> {
        self.terminal_control_calls.fetch_add(1, Ordering::SeqCst);
        self.repository_publication_rejections
            .lock()
            .unwrap()
            .push(rejection);
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionAgentCoordination for RecordingSessionControl {
    // Fixture rule P1: the signed transport fixture scripts Agent messages without root admission.
    // Prepared preparation refuses without changing any recorded effect.
    async fn prepare_prepared_session_run_activity(
        &self,
        _session: &awaken_session_types::SessionId,
        _agent: &str,
        _run: &awaken_agent_contract::agent::run::Id,
        _mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::unavailable(
            "the signed transport fixture scripts Agent messages without root admission",
        ))
    }

    // Fixture rule P1: the signed transport fixture scripts Agent messages without root admission.
    // Prepared admission refuses without changing any recorded effect.
    async fn admit_prepared_session_run_activity(
        &self,
        _session: &awaken_session_types::SessionId,
        _run: &awaken_agent_contract::agent::run::Id,
        _mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
        _preparation: awaken_session_execution_contract::PreparedSessionRunActivity,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivityAdmission,
        awaken_session_types::RunError,
    > {
        Err(awaken_session_types::RunError::unavailable(
            "the signed transport fixture scripts Agent messages without root admission",
        ))
    }

    async fn admit_session_run_activity(
        &self,
        _session_id: &SessionId,
        _agent_id: &str,
        _run_id: &RunId,
        _mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivityAdmission,
        awaken_session_types::RunError,
    > {
        Err(awaken_session_types::RunError::bad_request(
            "signed coordination fixture does not admit Session activity",
        ))
    }

    async fn admit_session_model_request(
        &self,
        _session_id: &SessionId,
        _thread_id: &ThreadId,
        _run_id: &RunId,
    ) -> Result<bool, awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::bad_request(
            "signed coordination fixture does not admit model requests",
        ))
    }

    async fn list_session_agents(
        &self,
        _session_id: &SessionId,
    ) -> Result<
        Vec<awaken_session_execution_contract::SessionAgentRosterEntry>,
        awaken_session_types::RunError,
    > {
        self.agent_lists.fetch_add(1, Ordering::SeqCst);
        Ok(vec![
            awaken_session_execution_contract::SessionAgentRosterEntry {
                agent_id: "researcher".into(),
                name: "Researcher".into(),
                description: Some("remote test agent".into()),
            },
        ])
    }

    async fn send_session_agent_message(
        &self,
        command: awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_types::RunError,
    > {
        self.agent_messages.lock().unwrap().push(command);
        Ok(
            awaken_session_execution_contract::SessionAgentMessageReceipt {
                thread_id: ThreadId("signed-child-thread".into()),
            },
        )
    }

    async fn settle_session_agent_boundary(
        &self,
        command: awaken_session_execution_contract::SessionAgentBoundaryCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        self.agent_boundaries.lock().unwrap().push(command);
        Ok(())
    }

    async fn finalize_session_agent_boundary(
        &self,
        _command: awaken_session_execution_contract::SessionAgentBoundaryCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::bad_request(
            "signed coordination fixture does not finalize post-Dispatch roots",
        ))
    }

    async fn interrupt_session_thread(
        &self,
        _session_id: &SessionId,
        _child_thread_id: &ThreadId,
    ) -> Result<(), awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::internal(
            "signed Worker transport does not own public Thread interruption",
        ))
    }

    async fn reply_session_thread_tool(
        &self,
        _command: awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::internal(
            "signed Worker transport does not own public Thread replies",
        ))
    }
}

#[derive(Default)]
struct TestWorkerDirectory(Mutex<Option<RegisteredWorker>>);

struct StaticEnvironmentWarmups(Vec<awaken_session_configuration_contract::EnvironmentSnapshot>);

#[async_trait::async_trait]
impl awaken_session_configuration_contract::EnvironmentWarmupSource for StaticEnvironmentWarmups {
    async fn current_environment_warmups(
        &self,
    ) -> Result<Vec<awaken_session_configuration_contract::EnvironmentSnapshot>, String> {
        Ok(self.0.clone())
    }
}

#[async_trait::async_trait]
impl WorkerDirectory for TestWorkerDirectory {
    async fn lock_incarnation(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<Option<awaken_worker_contract::WorkerIncarnationGuard>, RegistryError> {
        Err(RegistryError::Persistence(
            "fixture forbids physical admission".into(),
        ))
    }

    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        let record = RegisteredWorker {
            snapshot: WorkerSnapshot {
                identity: WorkerIdentity::new(
                    registration.worker_id,
                    registration.incarnation_id,
                    1,
                ),
                state: WorkerState::Starting,
                capability_fingerprint: registration.manifest.fingerprint().unwrap(),
                manifest: registration.manifest,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            },
            heartbeat_sequence: 0,
            observation_sequence: 0,
            registered_at_ms: now_ms,
            heartbeat_at_ms: now_ms,
            drain_deadline_ms: None,
            process_exit: None,
            create_ledger: None,
            source_retention: Default::default(),
        };
        *self.0.lock().unwrap() = Some(record.clone());
        Ok(record)
    }

    async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        let mut current = self.0.lock().unwrap();
        let Some(record) = current.as_mut() else {
            return Ok(RegistryMutation::NotFound);
        };
        if &record.snapshot.identity != identity {
            return Ok(RegistryMutation::StaleIncarnation);
        }
        let observations_changed = record.snapshot.credential_observations
            != heartbeat.credential_observations
            || record.snapshot.acp_capability_observations != heartbeat.acp_capability_observations;
        record.snapshot.state = if heartbeat.ready {
            WorkerState::Ready
        } else {
            WorkerState::Starting
        };
        record.snapshot.in_flight = heartbeat.in_flight;
        record.snapshot.warm_environment_shapes = heartbeat.warm_environment_shapes;
        record.snapshot.credential_observations = heartbeat.credential_observations;
        record.snapshot.acp_capability_observations = heartbeat.acp_capability_observations;
        if observations_changed {
            record.observation_sequence = heartbeat.sequence;
        }
        record.snapshot.expires_at_ms = now_ms.saturating_add(ttl_ms);
        record.heartbeat_sequence = heartbeat.sequence;
        record.heartbeat_at_ms = now_ms;
        Ok(RegistryMutation::Applied)
    }

    async fn begin_drain(
        &self,
        _identity: &WorkerIdentity,
        _deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn mark_quiesced(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn deregister(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn expire_incarnation(
        &self,
        _identity: &WorkerIdentity,
        _now_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::InvalidTransition)
    }

    async fn record_process_exit(
        &self,
        _identity: &WorkerIdentity,
        _receipt: awaken_dispatch::WorkerProcessExit,
    ) -> Result<RegistryMutation, RegistryError> {
        Err(RegistryError::Persistence(
            "process-exit persistence is outside this read-oriented fixture".into(),
        ))
    }
}

fn activation() -> RunActivation {
    RunActivation::new(
        RunId("signed-run".into()),
        ThreadId("signed-thread".into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("signed-snapshot".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("signed-agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("signed-catalog".into()),
                instructions: String::new(),
                max_steps: Some(1),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "genai"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("signed-snapshot-fingerprint".into()),
        },
        vec![Message::text(
            MessageId("signed-input".into()),
            Role::User,
            "run",
        )],
    )
}

fn standard_worker_dispatch(activation: RunActivation) -> RunDispatch {
    let mut dispatch = RunDispatch::new(activation);
    dispatch.placement.runtime_protocol_version = 1;
    // C0 current foreground codec metadata is authored by its single compiler;
    // protocol/signature/epoch/Work and lease negatives remain unchanged.
    dispatch.with_worker_codec_requirements()
}

/// Shared Worker-auth middleware decision table and FMECA:
/// F1 warmup projection accepts bootstrap/stale identity and leaks Environment
/// configuration (S8 O3 D3, RPN72); F2 warmup route invents a second auth decision
/// path (S8 O3 D4, RPN96). Both are mitigated by the existing signed middleware and
/// current-directory identity verifier used by all Worker control routes.
/// Causes: C1 valid route-bound signed bootstrap assertion -> registration only; C2 the
/// same bootstrap assertion used as an allocated incarnation -> reject before
/// dispatch; C3 valid incarnation-bound assertion -> heartbeat and dispatch
/// and warmup handlers receive one verified context; C4 invalid/replayed assertion
/// -> HTTP 401 before a handler. Effects: E1 register only, E2 reject, E3 return
/// current warmup projection. Constraint/Invariant: only a current allocated
/// incarnation may cross heartbeat or dispatch authority. Decision rule:
/// A1 C1->E1; A2 C2->E2; A3 C3->E3; A4 C4->E2 over real HTTP.
#[tokio::test(flavor = "multi_thread")]
async fn signed_identity_covers_register_heartbeat_and_dispatch() {
    let clock = Arc::new(ManualClock::new(10_000));
    let credential = WorkerSigningCredential::new(
        "signed-http-worker",
        "rotation-1",
        "credential-1",
        b"test-only-signing-secret".to_vec(),
    )
    .unwrap();
    let authenticator = SignedWorkerAuthenticator::new(credential.clone(), clock.clone())
        .with_time_policy(60_000, 0);
    let authenticator: Arc<dyn WorkerRequestAuthenticator> = Arc::new(authenticator);
    let directory = Arc::new(TestWorkerDirectory::default());
    // The dispatch port reads "now" from its store; mirror the service clock.
    let dispatch_clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(MemoryDispatchStore::new(dispatch_clock.clone()));
    let session_control = Arc::new(RecordingSessionControl {
        clock: Some(clock.clone()),
        ..Default::default()
    });
    *session_control.projection.lock().unwrap() = Some(frozen_projection());
    let session_work = Arc::new(RecordingSessionWorkAuthority::default());
    let environment_bindings = Arc::new(RecordingEnvironmentBindings::default());
    let live_stream = Arc::new(MemoryStreamSink::new());
    let service = WorkerDispatchService::new(
        dispatch.clone(),
        authenticator.clone(),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(30_000)),
    )
    .with_worker_directory(directory.clone(), 30_000)
    .with_session_control(session_control.clone())
    .with_session_coordination(session_control.clone())
    .with_terminal_cleanup_control(session_control.clone())
    .with_session_environment_bindings(environment_bindings.clone())
    .with_session_work_authority(session_work.clone())
    .with_stream_sink(live_stream.clone());
    let warmup = awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "signed-env".into(),
        revision: awaken_environment_contract::EnvironmentRevision(3),
        self_hosted: false,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "signed-shape".into(),
        ),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: Some("registry.example/env@sha256:signed".into()),
        network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    let router = dispatch_transport_router_with_service(Arc::new(service)).merge(
        worker_environment_warmup_router(
            Arc::new(StaticEnvironmentWarmups(vec![warmup])),
            directory.clone(),
            authenticator,
            clock.clone(),
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let unauthenticated = reqwest::Client::new()
        .post(format!("http://{address}/v1/worker/register"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("C4 unauthenticated request reaches middleware");
    assert_eq!(
        unauthenticated.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "C4"
    );

    let authorizer = Arc::new(
        SignedWorkerRequestAuthorizer::new(credential, clock.clone()).with_assertion_ttl_ms(10_000),
    );
    let bootstrap = WorkerUpstream::new(format!("http://{address}"))
        .with_worker_id("signed-http-worker")
        .with_request_authorizer(authorizer);
    let manifest = WorkerManifest {
        build_digest: "signed-http-build".into(),
        ..current_dispatch_codec_manifest()
    };
    // Worker lease receipt decision rules: successful registration and Applied
    // heartbeat both disclose the same positive Coordinator-owned TTL; a
    // bootstrap credential still cannot use incarnation-only operations.
    let registration_receipt = WorkerControlClient::new(bootstrap.clone())
        .register_classified("signed-http-boot", manifest)
        .await
        .expect("bootstrap assertion registers the Worker");
    assert_eq!(registration_receipt.lease_ttl_ms, 30_000);
    assert_eq!(
        registration_receipt.runtime_protocol_version,
        Some(awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION)
    );
    let registered = registration_receipt.worker;
    assert!(
        WorkerControlClient::new(bootstrap.clone())
            .heartbeat(
                &registered.snapshot.identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: Default::default(),
                    credential_observations: Default::default(),
                    acp_capability_observations: Default::default(),
                    source_retention: Default::default(),
                },
            )
            .await
            .is_err(),
        "a bootstrap assertion cannot act as an allocated incarnation"
    );
    assert!(
        WorkerControlClient::new(bootstrap.clone())
            .current_environment_warmups(&registered.snapshot.identity)
            .await
            .is_err(),
        "A2 bootstrap assertion cannot read the Worker warmup projection"
    );

    let upstream = bootstrap.with_worker_identity(registered.snapshot.identity.clone());
    let heartbeat = WorkerControlClient::new(upstream.clone())
        .heartbeat(
            &registered.snapshot.identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                source_retention: Default::default(),
            },
        )
        .await
        .expect("incarnation-bound assertion heartbeats");
    assert_eq!(heartbeat.mutation, RegistryMutation::Applied);
    assert_eq!(heartbeat.lease_ttl_ms, Some(30_000));
    assert_eq!(
        session_work.reconciliation_requests.load(Ordering::SeqCst),
        1,
        "Starting to Ready wakes the canonical Session lifecycle supervisor",
    );
    // Worker-evidence wake decision table: C1 an applied heartbeat changes a
    // placement coordinate => E1 wake once; C2 only sequence/lease time changes
    // => E2 no wake; C3 a rejected heartbeat => E3 no wake. The authenticated
    // transport and registry still own mutation validity; this hint owns no
    // state and the Session supervisor re-reads every authority.
    let unchanged = WorkerControlClient::new(upstream.clone())
        .heartbeat(
            &registered.snapshot.identity,
            WorkerHeartbeat {
                sequence: 2,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                source_retention: Default::default(),
            },
        )
        .await
        .expect("same placement evidence renews the registry lease");
    assert_eq!(unchanged.mutation, RegistryMutation::Applied, "C2");
    assert_eq!(
        session_work.reconciliation_requests.load(Ordering::SeqCst),
        1,
        "C2/E2 lease-only heartbeat does not defeat lifecycle backoff",
    );
    // Duplicate-path removal rule D1: an otherwise valid incarnation cannot
    // reach the retired owner-wide renewal route. Exact `/renew` remains the
    // only Run-lease transport, so this must be route absence rather than a
    // compatibility response.
    let retired_path = "/v1/worker/dispatch/renew_owned";
    let retired_request = upstream
        .authorize(
            "POST",
            retired_path,
            upstream
                .client()
                .post(format!("http://{address}{retired_path}")),
        )
        .expect("sign retired-route probe")
        .json(&serde_json::json!({"identity": &registered.snapshot.identity}));
    assert_eq!(
        retired_request
            .send()
            .await
            .expect("retired-route probe returns")
            .status(),
        reqwest::StatusCode::NOT_FOUND,
        "D1"
    );
    let warmups = WorkerControlClient::new(upstream.clone())
        .current_environment_warmups(&registered.snapshot.identity)
        .await
        .expect("A3 incarnation-bound assertion reads current warmups");
    assert_eq!(warmups.len(), 1, "A3");
    assert_eq!(warmups[0].environment_id, "signed-env", "A3");

    // Reuse the REAL default Worker transport factory and its two existing
    // roles, freezing the SAME registered manifest. No second client or codec
    // policy; signed-body mismatch/old protocol negatives remain fail-closed.
    let (queue, live_publisher) = awaken_coordinator_client::worker_transports_with_upstream(
        &upstream,
        registered.snapshot.identity.clone(),
        registered.snapshot.manifest.clone(),
    );
    // T31: the signed resume body has no resource selector. Only the guarded
    // durable dispatch supplies the exact pin, including explicit empty input.
    let carried_resources =
        awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
            "signed-workspace",
            7,
            Default::default(),
        );
    let mut request = standard_worker_dispatch(activation());
    request.session_resources = Some(awaken_dispatch::SessionResourceEnvelope::at_revision(
        carried_resources.workspace_id.clone(),
        carried_resources.revision,
        carried_resources.encode_resources().unwrap(),
    ));
    queue
        .enqueue(request)
        .await
        .expect("signed dispatch enqueue");
    let claimed = queue
        .claim(
            &registered.snapshot.identity.lease_owner(),
            30_000,
            &Default::default(),
        )
        .await
        .expect("signed dispatch claim")
        .expect("queued run is claimable");
    assert_eq!(
        claimed.lease.owner(),
        registered.snapshot.identity.lease_owner()
    );
    let claim = awaken_dispatch::RunClaim::from(&claimed.lease);
    assert!(
        queue
            .claim_is_current(&claim)
            .await
            .expect("signed exact-claim verification")
    );
    assert_eq!(
        session_work.acquisitions.load(Ordering::SeqCst),
        1,
        "claim admission acquires exactly one Session Work receipt"
    );
    attempt_binding::verify_ordinary_return_compatibility(
        queue.as_ref(),
        &upstream,
        &directory,
        &dispatch,
        &claim,
    )
    .await;

    // Cause graph: signed exact incarnation -> live registry lease -> identity
    // owns the exact Run claim -> guarded Run thread equals Session -> resume the
    // already-frozen projection while the epoch guard is held. Every failed
    // cause rejects before Control mutation.
    //
    // | Rule | Identity | Claim owner/epoch | Session=thread | Effect |
    // |---|---|---|---|---|
    // | T1 | exact/live | exact/live | yes | resume frozen projection |
    // | T2 | exact/live | wrong owner | yes | reject before Control |
    // | T3 | exact/live | exact/live | no | reject before Control |
    // | T4 | exact/live | stale/expired | yes | reject before Control |
    // | T5 | exact/live | Session lease exact | - | activate reaches Control |
    // | T6 | wrong incarnation | Session lease exact | - | reject before Control |
    // | T7 | exact/live | Session lease expired | - | reject before Control |
    // | T8 | exact/live | Session lease exact | - | acknowledge reaches Control |
    // | T9 | exact/live | Session lease exact | - | failure reaches Control |
    // | T10 | exact/live | exact asserted lease within registry lease | - | lease-only renew reaches Control |
    // | T11 | exact/live | asserted owner/incarnation mismatch | - | reject before Control |
    // | T12 | exact/live | desired renewal beyond registry lease | - | cap to registry expiry; renew reaches Control |
    // | T13 | exact/live | exact/live | wrong Session | reject resume before Control |
    // | T14 | exact/live | exact/live | frozen Session | mark claim-authorized reassignment |
    // | T15 | exact/live | requested expiry precedes assertion | - | reject contradictory authority |
    // | T16 | exact/live | explicit renewal | Control NotReady | preserve typed reply |
    // | T17 | exact/live | Work owned by other Worker | yes | reject resume before Control |
    // | T18 | exact/live | Work owner changes before phase | - | reject phase before Control |
    // | T19 | exact/live | exact Work owner | claim check | atomically renew Work |
    // | T20 | exact/live | exact Work owner | root settle | release Work, then settle Run |
    // | T21 | exact/live | active Work remains | deregister | release exact incarnation |
    // | T22 | child Run | parent Work exact | verify/resume | use parent Session affinity |
    // | T23 | child Run | borrowed parent Work | settle | retain Work for waiting parent |
    // | T24 | exact/live | retired Work is unowned | renewal | typed Retired; no Control call |
    // | T25 | root Run | exact claim/Session | list+send | one coordination application port |
    // | T26 | root Run | wrong Session/source | list+send | reject before application |
    // | T26b | child Run | valid parent affinity + forged root source | list+send | reject before application |
    // | T27 | async child | exact claim/epoch/Agent/cancel provenance | coordinate replay | deliver every retry to idempotent app |
    // | T28 | async child | stale claim/wrong Session/epoch/Agent/cancel provenance | settle | reject before application |
    // | T29 | exact signed claim | exact logical Thread | live delta | forward once |
    // | T30 | exact signed claim | forged logical Thread | live delta | reject/no forward |
    //
    // FMECA T24: a settled self-hosted Run retires Work before its longer
    // realization lease expires. Classifying that expected absence as another
    // Worker's ownership produces a false critical alarm and obscures the true
    // lifecycle edge; the transport now preserves Retired so the Worker uses
    // its one quiet local-projection retirement path.
    let live_event = |thread_id: &str| {
        StreamObservation::assistant_delta(
            claim.run_id.clone(),
            ThreadId(thread_id.into()),
            0,
            0,
            AgentEvent::Delta(Delta::TextDelta {
                delta: "live".into(),
            }),
        )
    };
    live_publisher
        .publish_observation(&claim, live_event("signed-thread"))
        .await
        .expect("T29 live publication remains best effort");
    assert_eq!(live_stream.events().len(), 1, "T29");
    live_publisher
        .publish_observation(&claim, live_event("forged-thread"))
        .await
        .expect("T30 rejected transport remains best effort to the Worker");
    assert_eq!(live_stream.events().len(), 1, "T30");

    let client = WorkerControlClient::new(upstream.clone());
    let work_receipt = claimed
        .session_work_authority
        .as_ref()
        .expect("registered Session claim carries exact Work authority")
        .clone();
    *session_work.owner.lock().unwrap() = Some("another-worker-incarnation".into());
    let unavailable = client
        .resume_session(
            &registered.snapshot.identity,
            &claim,
            "signed-thread",
            Some(&work_receipt),
            None,
        )
        .await
        .expect_err("T17");
    assert!(
        !unavailable.to_string().is_empty(),
        "T17 rejects stale Work authority"
    );
    assert!(session_control.begins.lock().unwrap().is_empty(), "T17");
    *session_work.owner.lock().unwrap() = None;
    let resumed = client
        .resume_session(
            &registered.snapshot.identity,
            &claim,
            "signed-thread",
            Some(&work_receipt),
            None,
        )
        .await
        .expect("T1 exact signed claim resumes")
        .expect("T1 frozen Session has a realization directive");
    assert_eq!(resumed.projection, frozen_projection(), "T1");
    assert_eq!(
        session_control.begins.lock().unwrap()[0]
            .expected_resources
            .as_ref(),
        Some(&carried_resources),
        "T31 exact guarded dispatch, not latest projection or client hint"
    );
    assert!(
        session_control.begins.lock().unwrap()[0]
            .target
            .reassign_existing_lease,
        "T14"
    );
    assert!(
        client
            .resume_session(
                &registered.snapshot.identity,
                &claim,
                "another-session",
                Some(&work_receipt),
                None,
            )
            .await
            .is_err(),
        "T13"
    );
    let mut wrong_owner = claim.clone();
    wrong_owner.owner = "another-owner".into();
    assert!(
        client
            .resume_session(
                &registered.snapshot.identity,
                &wrong_owner,
                "signed-thread",
                Some(&work_receipt),
                None,
            )
            .await
            .is_err(),
        "T2"
    );
    assert_eq!(session_control.begins.lock().unwrap().len(), 1, "T2/T13");

    let agents = client
        .list_session_agents(&registered.snapshot.identity, &claim, "signed-thread")
        .await
        .expect("T25 claimed roster");
    assert_eq!(agents.len(), 1, "T25");
    assert_eq!(session_control.agent_lists.load(Ordering::SeqCst), 1, "T25");
    assert!(
        client
            .list_session_agents(&registered.snapshot.identity, &claim, "another-session")
            .await
            .is_err(),
        "T26 wrong Session"
    );
    assert_eq!(session_control.agent_lists.load(Ordering::SeqCst), 1, "T26");

    let message = awaken_session_execution_contract::SessionAgentMessageCommand {
        session_id: "signed-thread".into(),
        source_thread_id: ThreadId("signed-thread".into()),
        source_run_id: claim.run_id.clone(),
        source_call_id: "call-send".into(),
        operation_id: "operation-send".into(),
        target: awaken_session_execution_contract::SessionAgentTarget::Spawn {
            agent_id: "researcher".into(),
        },
        message: "research this".into(),
    };
    let receipt = client
        .send_session_agent_message(&registered.snapshot.identity, &claim, message.clone())
        .await
        .expect("T25 claimed send");
    assert_eq!(
        receipt.thread_id,
        ThreadId("signed-child-thread".into()),
        "T25"
    );
    let mut wrong_source = message;
    wrong_source.source_run_id = RunId("wrong-source".into());
    assert!(
        client
            .send_session_agent_message(&registered.snapshot.identity, &claim, wrong_source)
            .await
            .is_err(),
        "T26 wrong source"
    );
    assert_eq!(
        session_control.agent_messages.lock().unwrap().len(),
        1,
        "T25/T26"
    );

    let realization_lease = resumed.authority.lease.clone();
    assert_eq!(
        resumed.authority.remaining_ms.get(),
        realization_lease.expires_at_unix_ms - 10_000,
        "receipt horizon comes from Control, not this machine's wall clock"
    );
    let session_client =
        WorkerControlSessionClient::new(client.clone(), registered.snapshot.identity.clone());

    // Environment-root transport cause/effect table. C1 is the authenticated
    // current Worker incarnation; C2 is an exact/live versus foreign realization
    // lease; C3 is the aggregate decision Authorized/AlreadyApplied/Unowned or
    // retryable unavailable; C4 is receipt persistence success/unavailability.
    // Effects: E1 only C1+C2 reaches the single binding authority; E2 preserves
    // the closed authorization enum over HTTP; E3 rejects Unowned remotely;
    // E4 preserves retryable classification; E5 persists one exact receipt and
    // returns the exact Store-read Environment authority through the production
    // Worker Session sink adapter, without reconstructing it client-side.
    //
    // | Rule | identity/lease | aggregate | persistence | Effect |
    // | B1 | exact | Authorized | n/a | E1 + Authorized |
    // | B2 | exact | AlreadyApplied | n/a | E1 + exact binding |
    // | B3 | exact | Unowned | n/a | reject before provider use |
    // | B4 | exact | unavailable | n/a | retryable 503 |
    // | B5 | foreign/stale | any | n/a | reject before binding authority |
    // | B6 | exact | Authorized | success/unavailable | one receipt + exact authority / retryable |
    let environment_intent = awaken_session_contract::SessionEnvironmentEffectIntent::new(
        "signed-thread",
        awaken_session_contract::SessionEnvironmentEffectKind::Create,
        Some(realization_lease.clone()),
    )
    .for_environment("env-fingerprint");
    assert_eq!(
        awaken_session_execution_contract::SessionEnvironmentBindingSink::authorize(
            &session_client,
            &environment_intent,
        )
        .await
        .expect("B1 authorized root effect"),
        awaken_session_contract::SessionEnvironmentEffectAuthorization::Authorized {
            intent: Box::new(environment_intent.clone())
        },
        "B1"
    );

    *environment_bindings.mode.lock().unwrap() =
        EnvironmentAuthorizationMode::AlreadyApplied("sandbox-binding-v2".into());
    assert_eq!(
        awaken_session_execution_contract::SessionEnvironmentBindingSink::authorize(
            &session_client,
            &environment_intent,
        )
        .await
        .expect("B2 response-loss readback"),
        awaken_session_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
            binding: "sandbox-binding-v2".into(),
            intent: Box::new(environment_intent.clone()),
        },
        "B2"
    );

    *environment_bindings.mode.lock().unwrap() = EnvironmentAuthorizationMode::Unowned;
    let unowned = awaken_session_execution_contract::SessionEnvironmentBindingSink::authorize(
        &session_client,
        &environment_intent,
    )
    .await
    .expect_err("B3 remote Worker cannot use an unowned root");
    assert_eq!(unowned.code, "session_environment_unowned", "B3");

    *environment_bindings.mode.lock().unwrap() = EnvironmentAuthorizationMode::Unavailable;
    let unavailable = awaken_session_execution_contract::SessionEnvironmentBindingSink::authorize(
        &session_client,
        &environment_intent,
    )
    .await
    .expect_err("B4 unavailable root authority remains retryable");
    assert_eq!(
        unavailable.kind,
        awaken_session_types::RunErrorKind::Unavailable,
        "B4"
    );
    assert_eq!(
        unavailable.code, "session_environment_authorization_unavailable",
        "B4"
    );

    *environment_bindings.mode.lock().unwrap() = EnvironmentAuthorizationMode::Authorized;
    let calls_before_foreign = environment_bindings.intents.lock().unwrap().len();
    let mut foreign_lease = realization_lease.clone();
    foreign_lease.runtime_incarnation = "foreign-incarnation".into();
    let foreign_intent = awaken_session_contract::SessionEnvironmentEffectIntent::new(
        "signed-thread",
        awaken_session_contract::SessionEnvironmentEffectKind::Create,
        Some(foreign_lease),
    )
    .for_environment("env-fingerprint");
    assert!(
        awaken_session_execution_contract::SessionEnvironmentBindingSink::authorize(
            &session_client,
            &foreign_intent,
        )
        .await
        .is_err(),
        "B5"
    );
    assert_eq!(
        environment_bindings.intents.lock().unwrap().len(),
        calls_before_foreign,
        "B5 foreign lease never reaches the binding authority"
    );

    let environment_receipt = awaken_session_contract::SessionEnvironmentReceipt::from_intent(
        &environment_intent,
        "sandbox-binding-v2",
    )
    .expect("B6 exact receipt");
    let persisted_environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding: "sandbox-binding-v2".into(),
        effect_id: Some(environment_receipt.effect_id.clone()),
        generation: Some(awaken_session_contract::SandboxGeneration::new(
            &awaken_session_types::SessionId::from("signed-thread"),
            100,
            1_000,
            "env-fingerprint",
            "base-image-fingerprint",
        )),
        idle_since_unix_ms: None,
    };
    *environment_bindings.persisted_environment.lock().unwrap() = persisted_environment.clone();
    let persisted = awaken_session_execution_contract::SessionEnvironmentBindingSink::persist(
        &session_client,
        environment_receipt.clone(),
    )
    .await
    .expect("B6 receipt persisted");
    assert_eq!(persisted, persisted_environment, "B6/E5");
    assert_eq!(
        environment_bindings.receipts.lock().unwrap().as_slice(),
        std::slice::from_ref(&environment_receipt),
        "B6"
    );
    environment_bindings
        .reject_persist
        .store(true, Ordering::SeqCst);
    let unavailable = awaken_session_execution_contract::SessionEnvironmentBindingSink::persist(
        &session_client,
        environment_receipt.clone(),
    )
    .await
    .expect_err("B6 retryable persistence failure");
    assert_eq!(
        unavailable.kind,
        awaken_session_types::RunErrorKind::Unavailable,
        "B6"
    );
    environment_bindings
        .reject_persist
        .store(false, Ordering::SeqCst);

    // Terminal-cleanup v2 transport cause/effect table: C1 the current
    // authenticated registry incarnation explicitly advertises runtime 1..2;
    // C2 identity/owner/incarnation is foreign or stale; C3 claim expiry is
    // within/beyond registry authority or requests reassignment; C4 Control projects an exact
    // preparation authorization bound to the polled effect; C5 Control projects
    // one exact disposal after preparation is durable; C6 a caller probes the
    // removed one-stage completion route. Generic realization renewal is already
    // covered by T10-T16; terminal cleanup owns no second renewal protocol.
    // Effects: K1 claim and poll return exact root snapshots; K2 and a
    // reassignment flag reject before Control; K3 an otherwise exact excessive
    // expiry is narrowed to current registry authority; K4 preparation
    // authorization and receipt bytes cross unchanged; K5 disposal effect and
    // receipt bytes cross unchanged; K6 the old route is absent and invokes no
    // Control method. Dispatch rows have already quiesced before targets freeze.
    //
    // | Rule | identity/capability | action | Effect |
    // | K1 | current + explicit v2 | claim/poll | exact assignment/action |
    // | K2 | stale/foreign | any | reject before Control |
    // | K3a | current + reassignment target | claim | reject before Control |
    // | K3b | current + excessive expiry | claim | cap to registry expiry |
    // | K3c/K3d | cursor valid/malformed | claim | cursor_protocol rows R1/R2 |
    // | K4 | current + explicit v2 | prepare authorize/record | exact bytes |
    // | K5 | current + explicit v2 | dispose authorize/record | exact bytes |
    // | K6 | current | old complete route | 404 + zero Control calls |
    let cleanup_target = awaken_session_execution_contract::SessionRealizationTarget {
        owner: registered.snapshot.identity.worker_id.clone(),
        runtime_incarnation: registered.snapshot.identity.lease_owner(),
        lease_expires_at_unix_ms: 30_000,
        reassign_existing_lease: false,
    };
    let assignment = client
        .claim_next_terminal_cleanup(&registered.snapshot.identity, cleanup_target.clone(), None)
        .await
        .expect("K1 cleanup recovery claim")
        .expect("K1 typed assignment");
    assert_eq!(assignment.session_id, "signed-terminal-recovery", "K1");
    assert_eq!(assignment.authority.lease.epoch, 9, "K1");
    assert_eq!(
        assignment.authority.remaining_ms.get(),
        assignment.authority.lease.expires_at_unix_ms - 10_000,
        "K1 authority horizon survives cleanup claim"
    );
    assert_eq!(
        session_control.cleanup_claims.lock().unwrap().len(),
        1,
        "K1"
    );
    let mut stale_identity = registered.snapshot.identity.clone();
    stale_identity.incarnation_id = "stale-boot".into();
    let mut stale_target = cleanup_target.clone();
    stale_target.runtime_incarnation = stale_identity.lease_owner();
    assert!(
        client
            .claim_next_terminal_cleanup(&stale_identity, stale_target, None)
            .await
            .is_err(),
        "K2 stale identity"
    );
    let mut foreign_target = cleanup_target.clone();
    foreign_target.owner = "foreign-worker".into();
    assert!(
        client
            .claim_next_terminal_cleanup(&registered.snapshot.identity, foreign_target, None)
            .await
            .is_err(),
        "K2 foreign owner"
    );
    let mut flagged_target = cleanup_target.clone();
    flagged_target.reassign_existing_lease = true;
    assert!(
        client
            .claim_next_terminal_cleanup(&registered.snapshot.identity, flagged_target, None)
            .await
            .is_err(),
        "K3 reassignment flag"
    );
    cursor_protocol::verify_claim_cursor_protocol(
        &upstream,
        address,
        &registered.snapshot.identity,
        &cleanup_target,
        &assignment.session_id,
        &session_control,
    )
    .await;

    let mut cleanup_operation = awaken_session_contract::SessionCleanupOperation::default();
    assert!(
        cleanup_operation.request(&awaken_session_types::SessionId::from("signed-thread")),
        "K4 fence"
    );
    cleanup_operation
        .freeze_targets(
            &awaken_session_types::SessionId::from("signed-thread"),
            [],
            0,
            0,
        )
        .expect("K4 root target");
    let cleanup_command = cleanup_operation
        .command_for(
            &awaken_session_types::SessionId::from("signed-thread"),
            "signed-thread",
        )
        .expect("K4 canonical command");
    *session_control.cleanup_action.lock().unwrap() = Some(
        awaken_session_contract::SessionTerminalCleanupAction::Prepare {
            commands: vec![cleanup_command.clone()],
        },
    );
    let cleanup_work = client
        .terminal_cleanup_work(
            &registered.snapshot.identity,
            "signed-thread",
            &realization_lease,
        )
        .await
        .expect("K4 cleanup poll")
        .expect("K4 terminal fence");
    assert_eq!(cleanup_work.assignment.session_id, "signed-thread", "K4");
    assert_eq!(
        cleanup_work.assignment.authority.lease, realization_lease,
        "K4"
    );
    assert_eq!(
        cleanup_work.assignment.authority.remaining_ms.get(),
        realization_lease.expires_at_unix_ms - 10_000,
        "K4 authority horizon survives cleanup poll"
    );
    assert_eq!(
        cleanup_work.action,
        awaken_session_contract::SessionTerminalCleanupAction::Prepare {
            commands: vec![cleanup_command.clone()],
        },
        "K4"
    );
    let preparation_effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        cleanup_command.clone(),
        realization_lease.clone(),
    );
    let inherited_provider_disposal =
        awaken_provisioning_contract::SandboxDisposalPreparation::new(
            awaken_provisioning_contract::SandboxEffectFence::new(
                "checkpoint-source-preparation",
                realization_lease.owner.clone(),
                realization_lease.runtime_incarnation.clone(),
                realization_lease.epoch.saturating_sub(1),
                realization_lease.expires_at_unix_ms.saturating_sub(1),
            )
            .unwrap(),
            "checkpoint-source-preparation-fingerprint",
        )
        .unwrap();
    let preparation_authorization =
        awaken_session_execution_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            preparation_effect.clone(),
            "workspace".into(),
            Some(inherited_provider_disposal.clone()),
        )
        .expect("K4 closed authorization");
    *session_control.preparation_authorization.lock().unwrap() =
        Some(preparation_authorization.clone());
    let transported_authorization = client
        .authorize_terminal_cleanup_effect(&registered.snapshot.identity, &preparation_effect)
        .await
        .expect("K4 preparation authorization");
    assert_eq!(
        transported_authorization, preparation_authorization,
        "K4 authorization bytes"
    );
    assert_eq!(
        session_control
            .authorized_preparations
            .lock()
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&preparation_effect),
        "K4 effect bytes"
    );
    let preparation = awaken_session_contract::SessionCleanupPreparation::try_new(
        &preparation_effect,
        preparation_effect.sandbox_effect_fence().unwrap(),
        Vec::new(),
    )
    .unwrap();
    client
        .record_terminal_cleanup_preparation(
            &registered.snapshot.identity,
            &realization_lease,
            preparation.clone(),
        )
        .await
        .expect("K4 preparation receipt");
    assert_eq!(
        session_control
            .cleanup_preparations
            .lock()
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&preparation),
        "K4 preparation bytes"
    );

    let disposal_effect_id = inherited_provider_disposal.operation_id().unwrap();
    let disposal_command: awaken_session_contract::SessionCleanupDisposalCommand =
        serde_json::from_value(serde_json::json!({
            "session_id": "signed-thread",
            "effect_id": disposal_effect_id,
            "preparation_fingerprint": "complete-terminal-preparation",
            "provider_disposal": inherited_provider_disposal,
        }))
        .expect("K5 disposal command fixture");
    *session_control.cleanup_action.lock().unwrap() = Some(
        awaken_session_contract::SessionTerminalCleanupAction::Dispose {
            command: disposal_command.clone(),
        },
    );
    let disposal_work = client
        .terminal_cleanup_work(
            &registered.snapshot.identity,
            "signed-thread",
            &realization_lease,
        )
        .await
        .expect("K5 disposal poll")
        .expect("K5 disposal work");
    assert_eq!(
        disposal_work.action,
        awaken_session_contract::SessionTerminalCleanupAction::Dispose {
            command: disposal_command.clone(),
        },
        "K5 disposal command bytes"
    );
    let disposal_effect = awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(
        disposal_command.clone(),
        realization_lease.clone(),
    );
    assert_eq!(
        client
            .authorize_terminal_cleanup_disposal(&registered.snapshot.identity, &disposal_effect,)
            .await
            .expect("K5 disposal authorization"),
        "workspace",
        "K5 Workspace"
    );
    assert_eq!(
        session_control
            .authorized_disposals
            .lock()
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&disposal_effect),
        "K5 effect bytes"
    );
    let disposal_receipt =
        awaken_session_contract::SessionCleanupDisposalReceipt::new(&disposal_command);
    client
        .record_terminal_cleanup_disposal(
            &registered.snapshot.identity,
            &realization_lease,
            disposal_receipt.clone(),
        )
        .await
        .expect("K5 disposal receipt");
    assert_eq!(
        session_control.cleanup_disposals.lock().unwrap().as_slice(),
        std::slice::from_ref(&disposal_receipt),
        "K5 receipt bytes"
    );

    let calls_before_old_route = session_control
        .terminal_control_calls
        .load(Ordering::SeqCst);
    let path = "/v1/worker/session/cleanup/complete";
    let response = upstream
        .authorize(
            "POST",
            path,
            upstream
                .client()
                .post(format!("{}{}", upstream.base_url(), path)),
        )
        .unwrap()
        .json(&serde_json::json!({
            "identity": &registered.snapshot.identity,
            "lease": &realization_lease,
            "completion": { "removed": true },
        }))
        .send()
        .await
        .expect("K6 removed completion route request");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND, "K6");
    assert_eq!(
        session_control
            .terminal_control_calls
            .load(Ordering::SeqCst),
        calls_before_old_route,
        "K6 zero Control calls"
    );
    let mut foreign_cleanup_lease = realization_lease.clone();
    foreign_cleanup_lease.owner = "foreign-worker".into();
    assert!(
        client
            .terminal_cleanup_work(
                &registered.snapshot.identity,
                "signed-thread",
                &foreign_cleanup_lease,
            )
            .await
            .is_err(),
        "K2 foreign completion lease"
    );

    // Publication transport cause/effect table. C1 the identity and cleanup
    // lease are current or foreign; C2 the route polls, completes, or rejects;
    // C3 Control has one command or no command. P1 current identity + exact
    // lease projects the one canonical command; P2 the same authority records
    // its canonical receipt; P3 it forwards the command-bound typed rejection;
    // P4 every C2 edge with a foreign lease is rejected before Control; P5 no
    // projected command stays pending without inventing work. Exact retries use
    // the same signed routes and outcomes; aggregate idempotency remains in
    // SessionCleanupOperation rather than this stateless transport.
    //
    // | Rule | Worker/lease | Route/Control state | Effect |
    // | P1 | current/exact | canonical | return exact command |
    // | P2 | current/exact | canonical | forward exact receipt |
    // | P3 | current/exact | canonical rejection | forward exact rejection |
    // | P4 | current/foreign | poll/complete/reject | reject before Control |
    // | P5 | current/exact | none | return pending None |
    let publication_intent: awaken_session_contract::SessionRepositoryPublicationIntent =
        serde_json::from_value(serde_json::json!({
            "input": {
                "binding_id": "source",
                "source": {
                    "kind": "repository",
                    "repository_id": "repo-1",
                    "config": {
                        "repository_id": "repo-1",
                        "version": 7,
                        "remote_url": "https://example.test/repo.git"
                    }
                },
                "mount_path": "/workspace/source",
                "access": "read_write"
            },
            "expectation": {
                "branch": "awf/work",
                "commit": "0123456789abcdef0123456789abcdef01234567"
            }
        }))
        .expect("publication intent fixture");
    let mut publication_operation = awaken_session_contract::SessionCleanupOperation::default();
    publication_operation
        .request_with_publication(
            &awaken_session_types::SessionId::from("signed-thread"),
            publication_intent.clone(),
        )
        .expect("P1 publication fence");
    publication_operation
        .freeze_targets(
            &awaken_session_types::SessionId::from("signed-thread"),
            [],
            0,
            0,
        )
        .expect("P1 root target");
    let publication_command = publication_operation
        .publication_command(&awaken_session_types::SessionId::from("signed-thread"))
        .expect("P1 command projection")
        .expect("P1 pending publication");
    let publication_projection =
        awaken_session_execution_contract::SessionRepositoryPublicationProjection {
            workspace_id: "workspace".into(),
            command: publication_command.clone(),
            current_lease: realization_lease.clone(),
        };
    *session_control
        .repository_publication_projection
        .lock()
        .unwrap() = Some(publication_projection.clone());
    let projected = client
        .terminal_repository_publication_command(
            &registered.snapshot.identity,
            "signed-thread",
            &realization_lease,
        )
        .await
        .expect("P1 publication poll")
        .expect("P1 command");
    assert_eq!(projected, publication_projection, "P1");
    let effect_receipt = serde_json::from_value(serde_json::json!({
        "repository_id": "repo-1",
        "source_remote_url": "https://example.test/repo.git",
        "branch": publication_intent.expectation.branch,
        "commit": publication_intent.expectation.commit,
    }))
    .expect("P2 effect receipt");
    let publication_receipt = awaken_session_contract::SessionRepositoryPublicationReceipt::new(
        &publication_command,
        effect_receipt,
    );
    client
        .record_terminal_repository_publication_receipt(
            &registered.snapshot.identity,
            "signed-thread",
            &realization_lease,
            publication_receipt.clone(),
        )
        .await
        .expect("P2 receipt");
    assert_eq!(
        session_control
            .repository_publication_receipts
            .lock()
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&publication_receipt),
        "P2"
    );
    let publication_rejection =
        awaken_session_contract::SessionRepositoryPublicationRejection::new(
            &publication_command,
            awaken_provisioning_contract::RepositoryPublicationRejection::RemoteRefChanged {
                observed_commit: "fedcba9876543210fedcba9876543210fedcba98".into(),
            },
        )
        .expect("P3 rejection");
    client
        .record_terminal_repository_publication_rejection(
            &registered.snapshot.identity,
            "signed-thread",
            &realization_lease,
            publication_rejection.clone(),
        )
        .await
        .expect("P3 rejection record");
    assert_eq!(
        session_control
            .repository_publication_rejections
            .lock()
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&publication_rejection),
        "P3"
    );
    // P3-source: the existing signed rejection endpoint transports the exact
    // closed-source cause without a second route. This fixture owns HTTP bytes
    // and lease/auth refusal only; aggregate/provider tests own source proof.
    let source_closed =
        awaken_session_contract::SessionRepositoryPublicationRejection::source_closed(
            &publication_command,
            "signed-source-binding",
            awaken_provisioning_contract::SandboxObservation::Terminal {
                physical_incarnation: "signed-source-pod".into(),
            },
        )
        .unwrap();
    client
        .record_terminal_repository_publication_rejection(
            &registered.snapshot.identity,
            "signed-thread",
            &realization_lease,
            source_closed.clone(),
        )
        .await
        .expect("P3-source rejection record");
    assert_eq!(
        session_control
            .repository_publication_rejections
            .lock()
            .unwrap()
            .as_slice(),
        &[publication_rejection.clone(), source_closed.clone()],
        "P3-source byte-identical cause"
    );
    let calls_before_foreign_publication = session_control
        .terminal_control_calls
        .load(Ordering::SeqCst);
    assert!(
        client
            .terminal_repository_publication_command(
                &registered.snapshot.identity,
                "signed-thread",
                &foreign_cleanup_lease,
            )
            .await
            .is_err(),
        "P4 foreign publication poll"
    );
    assert!(
        client
            .record_terminal_repository_publication_receipt(
                &registered.snapshot.identity,
                "signed-thread",
                &foreign_cleanup_lease,
                publication_receipt,
            )
            .await
            .is_err(),
        "P4 foreign publication completion"
    );
    assert!(
        client
            .record_terminal_repository_publication_rejection(
                &registered.snapshot.identity,
                "signed-thread",
                &foreign_cleanup_lease,
                publication_rejection,
            )
            .await
            .is_err(),
        "P4 foreign publication rejection"
    );
    assert!(
        client
            .record_terminal_repository_publication_rejection(
                &registered.snapshot.identity,
                "signed-thread",
                &foreign_cleanup_lease,
                source_closed,
            )
            .await
            .is_err(),
        "P4 foreign source-closure outcome"
    );
    assert_eq!(
        session_control
            .terminal_control_calls
            .load(Ordering::SeqCst),
        calls_before_foreign_publication,
        "P4 every publication route rejects before Control"
    );
    *session_control
        .repository_publication_projection
        .lock()
        .unwrap() = None;
    assert!(
        client
            .terminal_repository_publication_command(
                &registered.snapshot.identity,
                "signed-thread",
                &realization_lease,
            )
            .await
            .expect("P5 pending poll")
            .is_none(),
        "P5"
    );

    let renewal = awaken_session_execution_contract::RenewSessionRealization {
        session_id: "signed-thread".into(),
        asserted_lease: awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: registered.snapshot.identity.worker_id.clone(),
            runtime_incarnation: registered.snapshot.identity.lease_owner(),
            epoch: realization_lease.epoch,
            expires_at_unix_ms: realization_lease.expires_at_unix_ms,
        },
        requested_expires_at_unix_ms: realization_lease.expires_at_unix_ms,
    };
    let renewed = client
        .renew_session_realization(&registered.snapshot.identity, renewal.clone())
        .await
        .expect("T10");
    // R1 same injected Control clock + positive horizon => the exact lease and
    // remaining time survive the signed production router/client round trip.
    // R2..R7 malformed horizons are rejected in worker_control_client tests;
    // transport authentication/retirement rules below must remain unchanged.
    assert_eq!(renewed.lease, renewal.asserted_lease, "T10/R1");
    assert_eq!(
        renewed.remaining_ms.get(),
        renewed.lease.expires_at_unix_ms - 10_000,
        "T10/R1"
    );
    assert_eq!(session_control.renewals.lock().unwrap().len(), 1, "T10");
    *session_work.owner.lock().unwrap() = None;
    session_work.retired.store(true, Ordering::SeqCst);
    assert!(
        matches!(
            client
                .renew_session_realization(&registered.snapshot.identity, renewal.clone())
                .await,
            Err(awaken_session_execution_contract::SessionRealizationControlFailure::Retired)
        ),
        "T24"
    );
    assert_eq!(session_control.renewals.lock().unwrap().len(), 1, "T24");
    session_work.retired.store(false, Ordering::SeqCst);
    *session_work.owner.lock().unwrap() = Some(registered.snapshot.identity.lease_owner());
    error_classes::assert_renewal_failure_classes(
        &client,
        &session_control,
        &registered.snapshot.identity,
        renewal.clone(),
    )
    .await;
    let mut implicit = renewal.clone();
    implicit.asserted_lease.owner = "another-worker".into();
    assert!(
        client
            .renew_session_realization(&registered.snapshot.identity, implicit)
            .await
            .is_err(),
        "T11"
    );
    let mut excessive = renewal;
    excessive.requested_expires_at_unix_ms = u64::MAX;
    client
        .renew_session_realization(&registered.snapshot.identity, excessive.clone())
        .await
        .expect("T12 bounded renewal");
    assert_eq!(
        session_control.renewals.lock().unwrap().len(),
        11,
        "T11/T12 plus exactly nine fault matrix requests"
    );
    assert_eq!(
        session_control
            .renewals
            .lock()
            .unwrap()
            .last()
            .expect("T12 reaches Control")
            .requested_expires_at_unix_ms,
        registered.snapshot.expires_at_ms,
        "T12 never exceeds the authenticated registry lease"
    );
    let mut contradictory = excessive;
    contradictory.requested_expires_at_unix_ms =
        realization_lease.expires_at_unix_ms.saturating_sub(1);
    assert!(
        client
            .renew_session_realization(&registered.snapshot.identity, contradictory)
            .await
            .is_err(),
        "T15"
    );
    assert_eq!(
        session_control.renewals.lock().unwrap().len(),
        11,
        "T15/T16 no extra request"
    );
    // Continuation cause/effect table at the signed HTTP boundary:
    // RC1 exact live claim/Work/Worker + retained lease => exact renewal then
    // non-reassigning begin, preserving the generation. RC2 foreign owner or
    // incarnation / expiry beyond registry => zero Control calls. RC3 authority
    // rejects renewal => no begin and no fallback to assignment. RC4 a different
    // generation returned between renewal and begin => reject before Worker
    // projection installation. Existing T14 and retirement tests keep the
    // no-continuation successor behavior; none of these cases resend a Run.
    let continued = client
        .resume_session(
            &registered.snapshot.identity,
            &claim,
            "signed-thread",
            Some(&work_receipt),
            Some(&realization_lease),
        )
        .await
        .expect("RC1")
        .expect("RC1 directive");
    assert_eq!(
        continued.authority.lease.epoch, realization_lease.epoch,
        "RC1"
    );
    assert!(
        !session_control
            .begins
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .target
            .reassign_existing_lease,
        "RC1"
    );
    let begins_before_rejection = session_control.begins.lock().unwrap().len();
    let renewals_before_rejection = session_control.renewals.lock().unwrap().len();
    for dimension in ["owner", "incarnation", "expiry"] {
        let mut invalid = realization_lease.clone();
        match dimension {
            "owner" => invalid.owner = "foreign".into(),
            "incarnation" => invalid.runtime_incarnation = "foreign".into(),
            _ => invalid.expires_at_unix_ms = u64::MAX,
        }
        assert!(
            client
                .resume_session(
                    &registered.snapshot.identity,
                    &claim,
                    "signed-thread",
                    Some(&work_receipt),
                    Some(&invalid),
                )
                .await
                .is_err(),
            "RC2 {dimension}"
        );
    }
    assert_eq!(
        session_control.begins.lock().unwrap().len(),
        begins_before_rejection,
        "RC2"
    );
    assert_eq!(
        session_control.renewals.lock().unwrap().len(),
        renewals_before_rejection,
        "RC2"
    );
    *session_control.renewal_failure.lock().unwrap() =
        Some(awaken_session_execution_contract::SessionRealizationControlFailure::StaleOwnership);
    assert!(
        client
            .resume_session(
                &registered.snapshot.identity,
                &claim,
                "signed-thread",
                Some(&work_receipt),
                Some(&realization_lease),
            )
            .await
            .is_err(),
        "RC3"
    );
    *session_control.renewal_failure.lock().unwrap() = None;
    assert_eq!(
        session_control.begins.lock().unwrap().len(),
        begins_before_rejection,
        "RC3 no fallback"
    );
    // The recording port renews the asserted epoch but returns epoch 1 from
    // begin. This deliberately inconsistent provider exercises the final
    // generation check, not the real aggregate's CAS implementation.
    let mut changed = realization_lease.clone();
    changed.epoch += 1;
    assert!(
        client
            .resume_session(
                &registered.snapshot.identity,
                &claim,
                "signed-thread",
                Some(&work_receipt),
                Some(&changed),
            )
            .await
            .is_err(),
        "RC4"
    );
    *session_work.owner.lock().unwrap() = Some("another-worker-incarnation".into());
    assert!(
        client
            .activate_session_realization(
                &registered.snapshot.identity,
                awaken_session_execution_contract::ActivateSessionRealization {
                    session_id: "signed-thread".into(),
                    lease: realization_lease.clone(),
                    prepared_resource_revision: None,
                    mcp_receipts: Vec::new(),
                },
            )
            .await
            .is_err(),
        "T18"
    );
    assert_eq!(*session_control.activations.lock().unwrap(), 0, "T18");
    *session_work.owner.lock().unwrap() = Some(registered.snapshot.identity.lease_owner());
    client
        .activate_session_realization(
            &registered.snapshot.identity,
            awaken_session_execution_contract::ActivateSessionRealization {
                session_id: "signed-thread".into(),
                lease: realization_lease.clone(),
                prepared_resource_revision: None,
                mcp_receipts: Vec::new(),
            },
        )
        .await
        .expect("T5");
    assert_eq!(*session_control.activations.lock().unwrap(), 1, "T5");

    let wrong_identity = WorkerIdentity::new("signed-http-worker", "another-boot", 999);
    assert!(
        client
            .activate_session_realization(
                &wrong_identity,
                awaken_session_execution_contract::ActivateSessionRealization {
                    session_id: "signed-thread".into(),
                    lease: realization_lease.clone(),
                    prepared_resource_revision: None,
                    mcp_receipts: Vec::new(),
                },
            )
            .await
            .is_err(),
        "T6"
    );
    assert_eq!(*session_control.activations.lock().unwrap(), 1, "T6");

    let mut expired_lease = realization_lease.clone();
    expired_lease.expires_at_unix_ms = 0;
    assert!(
        client
            .activate_session_realization(
                &registered.snapshot.identity,
                awaken_session_execution_contract::ActivateSessionRealization {
                    session_id: "signed-thread".into(),
                    lease: expired_lease,
                    prepared_resource_revision: None,
                    mcp_receipts: Vec::new(),
                },
            )
            .await
            .is_err(),
        "T7"
    );
    assert_eq!(*session_control.activations.lock().unwrap(), 1, "T7");

    client
        .acknowledge_session_realization(
            &registered.snapshot.identity,
            awaken_session_execution_contract::AcknowledgeSessionRealization {
                session_id: "signed-thread".into(),
                lease: realization_lease.clone(),
                published: Vec::new(),
                drained: Vec::new(),
            },
        )
        .await
        .expect("T8");
    assert_eq!(*session_control.acknowledgements.lock().unwrap(), 1, "T8");

    client
        .fail_session_realization(
            &registered.snapshot.identity,
            awaken_session_execution_contract::FailSessionRealization {
                session_id: "signed-thread".into(),
                lease: realization_lease,
                prepared_resource_revision: None,
                retryable: false,
                not_ready: false,
                source_run_id: None,
                reason: "test failure".into(),
            },
        )
        .await
        .expect("T9");
    assert_eq!(*session_control.failures.lock().unwrap(), 1, "T9");

    // FMECA T22/T23: an outcome/delegated child has its own Run/thread
    // lifecycle but borrows the parent's Session Environment. Addressing Work
    // by the child thread deadlocks behind the waiting parent's one active
    // Environment lease; releasing the borrowed Work at child settlement
    // fences the still-running parent. The existing session_thread_id affinity
    // is therefore used for admission/resume, while only a root Run releases.
    let mut child_activation = activation();
    child_activation.run_id = RunId("signed-child-run".into());
    child_activation.thread_id = ThreadId("signed-child-thread".into());
    queue
        .enqueue(
            standard_worker_dispatch(child_activation)
                .for_session(ThreadId("signed-thread".into()))
                .with_session_activity_epoch(17),
        )
        .await
        .expect("T22 child dispatch");
    let child = queue
        .claim(
            &registered.snapshot.identity.lease_owner(),
            30_000,
            &Default::default(),
        )
        .await
        .expect("T22 child claim")
        .expect("T22 child is independently claimable");
    let child_claim = awaken_dispatch::RunClaim::from(&child.lease);
    session_work.acquired_sessions.lock().unwrap().clear();
    assert!(
        queue
            .claim_is_current(&child_claim)
            .await
            .expect("T22 child claim verification"),
        "T22"
    );
    assert!(
        client
            .list_session_agents(&registered.snapshot.identity, &child_claim, "signed-thread",)
            .await
            .is_err(),
        "T26b a child claim cannot list the primary roster"
    );
    let forged_primary_message = awaken_session_execution_contract::SessionAgentMessageCommand {
        session_id: "signed-thread".into(),
        source_thread_id: ThreadId("signed-thread".into()),
        source_run_id: child_claim.run_id.clone(),
        source_call_id: "forged-child-call".into(),
        operation_id: "forged-child-operation".into(),
        target: awaken_session_execution_contract::SessionAgentTarget::Spawn {
            agent_id: "researcher".into(),
        },
        message: "orphan this work".into(),
    };
    assert!(
        client
            .send_session_agent_message(
                &registered.snapshot.identity,
                &child_claim,
                forged_primary_message,
            )
            .await
            .is_err(),
        "T26b a child claim cannot impersonate the primary source Thread"
    );
    assert_eq!(
        session_control.agent_lists.load(Ordering::SeqCst),
        1,
        "T26b"
    );
    assert_eq!(
        session_control.agent_messages.lock().unwrap().len(),
        1,
        "T26b"
    );
    client
        .resume_session(
            &registered.snapshot.identity,
            &child_claim,
            "signed-thread",
            Some(&work_receipt),
            None,
        )
        .await
        .expect("T22 child resumes parent Session")
        .expect("T22 parent projection remains frozen");
    assert!(
        session_work
            .acquired_sessions
            .lock()
            .unwrap()
            .iter()
            .all(|session_id| session_id == "signed-thread"),
        "T22 every child Work check uses the parent Session"
    );

    let awaiting_boundary = awaken_session_execution_contract::SessionAgentBoundaryCommand {
        session_id: "signed-thread".into(),
        source_thread_id: ThreadId("signed-child-thread".into()),
        source_run_id: child_claim.run_id.clone(),
        source_agent_id: "signed-agent".into(),
        session_activity_epoch: 17,
        cancellation_requested: false,
    };
    // T27a causes: valid signed claim + exact Session coordinates + occupied
    // physical slot. Effect: reject before the Session application can advance
    // activity or report a child result. T27b exact return ACK clears only this
    // slot, after which the original idempotent boundary retries are admitted.
    // Neither claim validity nor a caller's terminal assertion replaces ACK.
    assert_eq!(
        queue.begin_attempt(&child_claim, None).await.unwrap(),
        AttemptAdmission::Applied
    );
    assert!(
        client
            .settle_session_agent_boundary(
                &registered.snapshot.identity,
                &child_claim,
                awaiting_boundary.clone(),
            )
            .await
            .is_err(),
        "T27a occupied slot"
    );
    assert!(
        session_control.agent_boundaries.lock().unwrap().is_empty(),
        "T27a no application effect"
    );
    assert_eq!(
        queue.finish_attempt(&child_claim, None).await.unwrap(),
        SettleOutcome::Applied,
        "T27b exact return"
    );
    client
        .settle_session_agent_boundary(
            &registered.snapshot.identity,
            &child_claim,
            awaiting_boundary.clone(),
        )
        .await
        .expect("T27 boundary coordinate");
    let ended_boundary = awaiting_boundary.clone();
    for _ in 0..2 {
        client
            .settle_session_agent_boundary(
                &registered.snapshot.identity,
                &child_claim,
                ended_boundary.clone(),
            )
            .await
            .expect("T27 terminal exact retry reaches the idempotent application");
    }
    assert_eq!(
        session_control.agent_boundaries.lock().unwrap().len(),
        3,
        "T27"
    );

    let mut wrong_epoch = ended_boundary.clone();
    wrong_epoch.session_activity_epoch = 18;
    assert!(
        client
            .settle_session_agent_boundary(
                &registered.snapshot.identity,
                &child_claim,
                wrong_epoch,
            )
            .await
            .is_err(),
        "T28 wrong activity epoch"
    );
    let mut forged_cancellation = ended_boundary.clone();
    forged_cancellation.cancellation_requested = true;
    assert!(
        client
            .settle_session_agent_boundary(
                &registered.snapshot.identity,
                &child_claim,
                forged_cancellation,
            )
            .await
            .is_err(),
        "T28 cancellation provenance must match the claim-fenced queue row"
    );
    let mut forged_agent = ended_boundary.clone();
    forged_agent.source_agent_id = "another-agent".into();
    assert!(
        client
            .settle_session_agent_boundary(
                &registered.snapshot.identity,
                &child_claim,
                forged_agent,
            )
            .await
            .is_err(),
        "T28 Agent provenance must match the claim-fenced queue snapshot"
    );
    let mut wrong_session = ended_boundary.clone();
    wrong_session.session_id = "another-session".into();
    assert!(
        client
            .settle_session_agent_boundary(
                &registered.snapshot.identity,
                &child_claim,
                wrong_session,
            )
            .await
            .is_err(),
        "T28 wrong parent Session"
    );
    let mut stale_child_claim = child_claim.clone();
    stale_child_claim.epoch += 1;
    assert!(
        client
            .settle_session_agent_boundary(
                &registered.snapshot.identity,
                &stale_child_claim,
                ended_boundary,
            )
            .await
            .is_err(),
        "T28 stale child claim"
    );
    assert_eq!(
        session_control.agent_boundaries.lock().unwrap().len(),
        3,
        "T28"
    );

    let releases_before_child = session_work.releases.load(Ordering::SeqCst);
    queue
        .settle(
            &child_claim.run_id,
            child_claim.epoch,
            awaken_dispatch::DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("T23 child settlement");
    assert_eq!(
        session_work.releases.load(Ordering::SeqCst),
        releases_before_child,
        "T23 child settlement retains the parent's Work"
    );

    // FMECA T20 causal graph: C1 one active Work is permitted per Environment;
    // C2 root Run settles; C3 another Session is queued. If C2 keeps the first
    // Work active, C1 blocks C3 forever. The root release is therefore part of
    // settlement; a later activity on the same Session can revive its stable
    // Work item, while T23 proves a child Run cannot release the parent fence.
    assert_eq!(
        queue
            .settle(
                &claim.run_id,
                claim.epoch,
                awaken_dispatch::DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("T20 signed settlement"),
        awaken_dispatch::SettleOutcome::Applied,
        "T20"
    );
    assert_eq!(
        session_work.releases.load(Ordering::SeqCst),
        1,
        "T20 root settlement releases the Environment ownership fence"
    );
    assert_eq!(
        session_work.released_sessions.lock().unwrap().as_slice(),
        ["signed-thread"],
        "T20 releases only the root Session Work"
    );

    let mut next_activation = activation();
    next_activation.run_id = RunId("signed-run-next".into());
    next_activation.thread_id = ThreadId("signed-thread-next".into());
    queue
        .enqueue(standard_worker_dispatch(next_activation))
        .await
        .expect("post-settlement claim fixture");
    let claimed = queue
        .claim(
            &registered.snapshot.identity.lease_owner(),
            30_000,
            &Default::default(),
        )
        .await
        .expect("post-settlement claim")
        .expect("released Session Work permits the next Session Run");
    let claim = awaken_dispatch::RunClaim::from(&claimed.lease);
    assert!(
        queue
            .claim_is_current(&claim)
            .await
            .expect("T20 next Session claim remains current"),
        "T20"
    );

    let begins_before_expired_claim = session_control.begins.lock().unwrap().len();
    clock.set(20_000);
    dispatch_clock.set(20_000);
    WorkerControlClient::new(upstream.clone())
        .heartbeat(
            &registered.snapshot.identity,
            WorkerHeartbeat {
                sequence: 3,
                ready: true,
                in_flight: 1,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                source_retention: Default::default(),
            },
        )
        .await
        .expect("registry lease remains live while the dispatch lease expires");
    clock.set(claimed.lease.expires_ms() + 1);
    dispatch_clock.set(claimed.lease.expires_ms() + 1);
    assert!(
        !queue
            .claim_is_current(&claim)
            .await
            .expect("signed expired-claim verification")
    );
    assert!(
        WorkerControlClient::new(upstream.clone())
            .resume_session(
                &registered.snapshot.identity,
                &claim,
                "signed-thread",
                Some(&work_receipt),
                None,
            )
            .await
            .is_err(),
        "T4"
    );
    assert_eq!(
        session_control.begins.lock().unwrap().len(),
        begins_before_expired_claim,
        "T4"
    );

    // T22: a registered Worker that loses subordinate admission can return its
    // exact unstarted claim immediately; a replay from the prior epoch is fenced.
    assert!(
        queue
            .relinquish_claim(&claim)
            .await
            .expect("signed relinquish")
            .applied(),
        "T22"
    );
    WorkerControlClient::new(upstream.clone())
        .heartbeat(
            &registered.snapshot.identity,
            WorkerHeartbeat {
                sequence: 4,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                source_retention: Default::default(),
            },
        )
        .await
        .expect("relinquish removes the Run from Worker in-flight capacity");
    let replacement = queue
        .claim(
            &registered.snapshot.identity.lease_owner(),
            30_000,
            &Default::default(),
        )
        .await
        .expect("reclaim relinquished Run")
        .expect("relinquished Run is pending without waiting for lease expiry");
    assert_eq!(replacement.lease.epoch(), claim.epoch + 1, "T22");
    assert!(
        !queue
            .relinquish_claim(&claim)
            .await
            .expect("stale relinquish is a fenced no-op")
            .applied(),
        "T22"
    );
    // T23: a failed attempt spends one attempt of the Coordinator's retry
    // budget (default 5); the Worker-supplied limit is ignored, so a limit of
    // one cannot exhaust the Run from the Worker side.
    let failed_claim = awaken_dispatch::RunClaim::from(&replacement.lease);
    assert!(
        queue
            .retry_claim(&failed_claim, 0, 1)
            .await
            .expect("signed failed-attempt release")
            .applied(),
        "T23"
    );
    let attempts = |dispatch: Arc<MemoryDispatchStore>, run: RunId| async move {
        let row = dispatch
            .list_dispatches()
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.run_id == run)
            .expect("T23 the Run is retained");
        (row.state, row.attempt_count)
    };
    assert_eq!(
        attempts(dispatch.clone(), failed_claim.run_id.clone()).await,
        (awaken_dispatch::DispatchState::Pending, 1),
        "T23 one attempt spent against the Coordinator's limit"
    );
    let replacement = queue
        .claim(
            &registered.snapshot.identity.lease_owner(),
            30_000,
            &Default::default(),
        )
        .await
        .expect("reclaim the retried Run")
        .expect("the retried Run is due after its zero delay");
    assert!(
        !queue
            .retry_claim(&failed_claim, 0, 1)
            .await
            .expect("stale failed-attempt release is a fenced no-op")
            .applied(),
        "T23"
    );
    let replacement_claim = awaken_dispatch::RunClaim::from(&replacement.lease);
    assert!(
        queue
            .defer_claim(&replacement_claim, i64::MAX as u64)
            .await
            .expect("signed request-local deferral")
            .applied(),
        "the authenticated internal Worker transport preserves request-local retry"
    );
    assert_eq!(
        attempts(dispatch.clone(), replacement_claim.run_id.clone()).await,
        (awaken_dispatch::DispatchState::Pending, 1),
        "T23 a deferral keeps the budget"
    );

    // FMECA T21: graceful restart can begin after the committed answer becomes
    // visible but before asynchronous Run settlement. Deregistration is the last
    // exact-incarnation boundary and releases that residual Work immediately;
    // crash recovery deliberately remains TTL-based.
    *session_work.owner.lock().unwrap() = Some(registered.snapshot.identity.lease_owner());
    WorkerControlClient::new(upstream.clone())
        .deregister(&registered.snapshot.identity)
        .await
        .expect("T21 exact deregistration");
    assert_eq!(session_work.releases.load(Ordering::SeqCst), 2, "T21");
}

#[tokio::test]
async fn protocol_admission_precedes_registration_and_heartbeat_mutation() {
    // Causes C1 explicit current range, C2 exact current identity, C3 operation.
    // R1 register && !C1 => 400, no registry record; register && C1 => receipt
    // selects current protocol. R2 old persisted registration + heartbeat =>
    // 400, unchanged TTL/state/sequence. R3 stale identity => normal stale result,
    // never mutates a replacement. R4 old exact drain/quiesce/deregister remains
    // callable: protocol denial cannot remove the existing retirement route.
    let directory = Arc::new(TestWorkerDirectory::default());
    // The dispatch port reads "now" from its store; mirror the service clock.
    let dispatch_clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let service = WorkerDispatchService::local(
        Arc::new(MemoryDispatchStore::new(dispatch_clock.clone())),
        awaken_time::system_clock(),
    )
    .with_worker_directory(directory.clone(), 30_000);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    for (min, max, admitted) in [
        (0, u32::MAX, false),
        (0, 3, false),
        (1, 1, false),
        (1, 2, false),
        (3, 2, false),
        (1, 3, false),
        (3, 3, false),
        (
            awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION + 1,
            awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION + 1,
            false,
        ),
        (1, awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION, true),
        (
            awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION,
            awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION,
            true,
        ),
    ] {
        *directory.0.lock().unwrap() = None;
        let mut manifest = WorkerManifest::default();
        manifest.runtime_protocol.min = min;
        manifest.runtime_protocol.max = max;
        let registration = WorkerRegistration {
            worker_id: "protocol-worker".into(),
            incarnation_id: "protocol-boot".into(),
            manifest,
            create_ledger: None,
        };
        let response = client
            .post(format!("http://{address}/v1/worker/register"))
            .header("x-awaken-worker-id", "protocol-worker")
            .json(&serde_json::json!({ "registration": &registration }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().is_success(), admitted, "{min}..{max}");
        if admitted {
            let receipt: awaken_dispatch_contract::WorkerRegistrationReceipt =
                response.json().await.unwrap();
            assert_eq!(
                receipt.runtime_protocol_version,
                Some(awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION)
            );
            continue;
        }
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(directory.0.lock().unwrap().is_none(), "R1 no mutation");
        // Seed a pre-upgrade persisted identity through the Directory, not the
        // HTTP admission path under test.
        let record = directory.register(registration, 0, u64::MAX).await.unwrap();
        let identity = record.snapshot.identity.clone();
        let heartbeat = WorkerHeartbeat {
            sequence: 1,
            ready: true,
            in_flight: 0,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            source_retention: Default::default(),
        };
        let response = client
            .post(format!("http://{address}/v1/worker/heartbeat"))
            .header("x-awaken-worker-id", "protocol-worker")
            .json(&serde_json::json!({ "identity": &identity, "heartbeat": &heartbeat }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(*directory.0.lock().unwrap(), Some(record.clone()), "R2");
        let mut stale = identity.clone();
        stale.generation += 1;
        let response = client
            .post(format!("http://{address}/v1/worker/heartbeat"))
            .header("x-awaken-worker-id", "protocol-worker")
            .json(&serde_json::json!({ "identity": stale, "heartbeat": &heartbeat }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["mutation"],
            "stale_incarnation"
        );
        assert_eq!(*directory.0.lock().unwrap(), Some(record), "R3");
        for operation in ["drain", "quiesced", "deregister"] {
            let response = client
                .post(format!("http://{address}/v1/worker/{operation}"))
                .header("x-awaken-worker-id", "protocol-worker")
                .json(&serde_json::json!({ "identity": &identity }))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success(), "R4/{operation}");
        }
    }
    server.abort();
}

/// C1 pre-upgrade registration has omitted/old/future/inverted protocol range;
/// C2 structurally valid ordinary claim/renew/resume or terminal effect request.
/// V1=C1+C2 => protocol-specific HTTP 400 before Control/Work/registry mutation.
/// Cross every range with every edge: malformed request rejection must not
/// masquerade as protocol rejection. Positive current protocol is exercised
/// by signed_identity_covers_register_heartbeat_and_dispatch.
#[tokio::test]
async fn incompatible_registration_rejected_before_execution_and_terminal_edges() {
    let identity = WorkerIdentity::new("any-worker", "any-incarnation", 1);
    let manifest = WorkerManifest::default();
    let directory = Arc::new(TestWorkerDirectory::default());
    *directory.0.lock().unwrap() = Some(RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: identity.clone(),
            state: WorkerState::Ready,
            capability_fingerprint: manifest.fingerprint().unwrap(),
            manifest,
            in_flight: 0,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms: u64::MAX,
        },
        heartbeat_sequence: 0,
        observation_sequence: 0,
        registered_at_ms: 0,
        heartbeat_at_ms: 0,
        drain_deadline_ms: None,
        process_exit: None,
        create_ledger: None,
        source_retention: Default::default(),
    });
    let session_control = Arc::new(RecordingSessionControl::default());
    // A real queued v1 request must remain pending after every denial; an
    // empty queue would not expose accidentally claiming before rejection.
    // The dispatch port reads "now" from its store; mirror the service clock.
    let dispatch_clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(MemoryDispatchStore::new(dispatch_clock.clone()));
    dispatch
        .enqueue(standard_worker_dispatch(activation()))
        .await
        .unwrap();
    let queued_before = dispatch.list_dispatches().await.unwrap();
    let service = WorkerDispatchService::local(dispatch.clone(), awaken_time::system_clock())
        .with_worker_directory(directory.clone(), 30_000)
        .with_session_control(session_control.clone());
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
        owner: identity.worker_id.clone(),
        runtime_incarnation: identity.lease_owner(),
        epoch: 2,
        expires_at_unix_ms: u64::MAX,
    };
    let command = awaken_session_contract::SessionCleanupCommand::new(
        &awaken_session_types::SessionId::from("any-session"),
        "any-session",
        "any-root",
    );
    let preparation_effect =
        awaken_session_contract::SessionTerminalCleanupEffect::new(command, lease.clone());
    let preparation = awaken_session_contract::SessionCleanupPreparation::try_new(
        &preparation_effect,
        preparation_effect.sandbox_effect_fence().unwrap(),
        Vec::new(),
    )
    .unwrap();
    let provider_preparation = awaken_provisioning_contract::SandboxDisposalPreparation::new(
        awaken_provisioning_contract::SandboxEffectFence::new(
            "any-provider-preparation",
            identity.worker_id.clone(),
            identity.lease_owner(),
            1,
            u64::MAX - 1,
        )
        .unwrap(),
        "any-provider-fingerprint",
    )
    .unwrap();
    let disposal_command: awaken_session_contract::SessionCleanupDisposalCommand =
        serde_json::from_value(serde_json::json!({
            "session_id": "any-session",
            "effect_id": provider_preparation.operation_id().unwrap(),
            "preparation_fingerprint": "any-complete-preparation",
            "provider_disposal": provider_preparation,
        }))
        .unwrap();
    let disposal_effect = awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(
        disposal_command.clone(),
        lease.clone(),
    );
    let disposal_receipt =
        awaken_session_contract::SessionCleanupDisposalReceipt::new(&disposal_command);
    let publication_receipt: awaken_session_contract::SessionRepositoryPublicationReceipt =
        serde_json::from_value(serde_json::json!({
            "command_fingerprint": "command",
            "effect_receipt": {
                "repository_id": "repository",
                "source_remote_url": "https://example.test/repository.git",
                "branch": "awf/work",
                "commit": "0123456789abcdef0123456789abcdef01234567"
            },
            "receipt_fingerprint": "receipt"
        }))
        .unwrap();
    let publication_rejection: awaken_session_contract::SessionRepositoryPublicationRejection =
        serde_json::from_value(serde_json::json!({
            "command_fingerprint": "command",
            "effect_rejection": {
                "reason": "remote_ref_absent"
            },
            "rejection_fingerprint": "rejection"
        }))
        .unwrap();
    let target = awaken_session_execution_contract::SessionRealizationTarget {
        owner: identity.worker_id.clone(),
        runtime_incarnation: identity.lease_owner(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let rules = [
        (
            "physical-begin",
            "/v1/worker/dispatch/attempt/begin",
            serde_json::json!({ "identity": &identity,
                "claim": { "run_id": "old-run", "owner": identity.lease_owner(), "epoch": 1 } }),
        ),
        (
            "physical-handoff",
            "/v1/worker/dispatch/attempt/handoff",
            serde_json::json!({ "identity": &identity,
                "claim": { "run_id": "old-run", "owner": identity.lease_owner(), "epoch": 1 } }),
        ),
        (
            "ordinary-claim",
            "/v1/worker/dispatch/claim",
            serde_json::json!({ "identity": &identity }),
        ),
        (
            "ordinary-renew",
            "/v1/worker/dispatch/renew",
            serde_json::json!({ "identity": &identity, "run_id": "old-run", "lease_epoch": 1 }),
        ),
        (
            "session-resume",
            "/v1/worker/session/resume",
            serde_json::json!({ "identity": &identity, "session_id": "any-session",
                "claim": { "run_id": "old-run", "owner": identity.lease_owner(), "epoch": 1 } }),
        ),
        (
            "session-renew",
            "/v1/worker/session/realization/renew",
            serde_json::json!({ "identity": &identity, "command": {
                "session_id": "any-session", "asserted_lease": &lease,
                "requested_expires_at_unix_ms": u64::MAX } }),
        ),
        (
            "claim",
            "/v1/worker/session/cleanup/claim-next",
            serde_json::json!({ "identity": &identity, "target": target,
                "sweep_cursor_version": awaken_session_execution_contract::TERMINAL_CLEANUP_SWEEP_CURSOR_VERSION,
                "after_session_id": null }),
        ),
        (
            "poll",
            "/v1/worker/session/cleanup/poll",
            serde_json::json!({ "identity": &identity, "session_id": "any-session", "lease": &lease }),
        ),
        (
            "preparation-authorize",
            "/v1/worker/session/cleanup/preparation/authorize",
            serde_json::json!({ "identity": &identity, "effect": &preparation_effect }),
        ),
        (
            "preparation-record",
            "/v1/worker/session/cleanup/preparation/record",
            serde_json::json!({ "identity": &identity, "lease": &lease, "preparation": preparation }),
        ),
        (
            "disposal-authorize",
            "/v1/worker/session/cleanup/disposal/authorize",
            serde_json::json!({ "identity": &identity, "effect": disposal_effect }),
        ),
        (
            "disposal-record",
            "/v1/worker/session/cleanup/disposal/record",
            serde_json::json!({ "identity": &identity, "lease": &lease, "receipt": disposal_receipt }),
        ),
        (
            "publication-poll",
            "/v1/worker/session/cleanup/repository-publication/poll",
            serde_json::json!({ "identity": &identity, "session_id": "any-session", "lease": &lease }),
        ),
        (
            "publication-record",
            "/v1/worker/session/cleanup/repository-publication/complete",
            serde_json::json!({
                "identity": &identity,
                "session_id": "any-session",
                "lease": &lease,
                "receipt": publication_receipt,
            }),
        ),
        (
            "publication-reject",
            "/v1/worker/session/cleanup/repository-publication/reject",
            serde_json::json!({
                "identity": &identity,
                "session_id": "any-session",
                "lease": &lease,
                "rejection": publication_rejection,
            }),
        ),
    ];
    for (min, max) in [
        (0, u32::MAX),
        (0, 3),
        (1, 1),
        (1, 2),
        (3, 2),
        (1, 3),
        (3, 3),
        (
            awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION + 1,
            awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION + 1,
        ),
    ] {
        {
            let mut current = directory.0.lock().unwrap();
            let snapshot = &mut current.as_mut().unwrap().snapshot;
            snapshot.manifest.runtime_protocol.min = min;
            snapshot.manifest.runtime_protocol.max = max;
            snapshot.capability_fingerprint = snapshot.manifest.fingerprint().unwrap();
        }
        let before = directory.0.lock().unwrap().clone();
        for (rule, path, body) in &rules {
            let response = reqwest::Client::new()
                .post(format!("http://{address}{path}"))
                .header("x-awaken-worker-id", &identity.worker_id)
                .json(body)
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::BAD_REQUEST,
                "{rule}"
            );
            assert!(
                response
                    .text()
                    .await
                    .unwrap()
                    .contains("current runtime protocol"),
                "{min}..{max}/{rule}: rejection must come from protocol admission"
            );
        }
        assert_eq!(*directory.0.lock().unwrap(), before);
        assert_eq!(dispatch.list_dispatches().await.unwrap(), queued_before);
    }
    assert!(session_control.begins.lock().unwrap().is_empty());
    assert!(session_control.renewals.lock().unwrap().is_empty());
    assert_eq!(
        session_control
            .terminal_control_calls
            .load(Ordering::SeqCst),
        0,
        "V1/E1"
    );
}
