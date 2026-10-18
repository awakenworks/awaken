// @formal entrypoint session.cleanup_completion_requires_exact_identity_and_canonical_artifact_evidence
//! `awaken-session-executor` — authority-free shared execution host infrastructure.
//!
//! It owns protocol-neutral execution coordination, capability assembly, and
//! the [`SharedHost`] used by every
//! driving protocol. [`ManagedHost`] and [`RunApplicationHost`] adapt Managed
//! Agents and AI SDK / AG-UI / A2A onto that one host, so they cannot create
//! protocol-specific run, resume, or terminal-state implementations.
//!
//! This crate consumes capabilities selected by process startup. It never opens
//! or selects a SQL/filesystem authority Store, never owns a public protocol or
//! domain aggregate, and never decides the deployment topology. Session resource
//! effects are supplied by `awaken-session-realization`. Coordinator
//! injects the sole durable [`RuntimeAuthority`]; a database-less Worker is
//! constructed with an [`UpstreamAuthority`] carrying its claim-fenced
//! transports. Product composition mounts the corresponding routers around
//! these protocol-neutral behaviors.

mod acp_backend;
mod acp_capability_probe;
mod acp_tool_export;
mod agent_catalog;
mod agent_runner;
mod application;
pub use application::SessionRealizationRevocation;

/// Opaque host-boundary proof that the canonical dispatch pool installed its
/// absorbing local drain fence. The private field carries the ingress owner's
/// token without exposing that infrastructure crate as a Runtime Host facade.
#[derive(Debug)]
#[must_use]
pub struct PoolDrainFence {
    _ingress: awaken_dispatch_execution::PoolDrainFence,
}

mod authority;
mod auxiliary_sandbox;
mod background;
mod background_task;
mod capabilities;
mod commit_ingest;
mod compact;
mod config;
mod coordination;
mod delegate;
mod deployment_config;
mod durable_operations;
mod host_error;
mod session_effects;

#[cfg(test)]
mod environment_effects;
mod host;
mod hub;
mod inference_routing;
mod installed_session;
mod judge;
mod lazy_sandbox;
mod live_inbox;
pub use live_inbox::LiveInboxes;
mod managed_adapter_error;
mod managed_host_composition;
pub use managed_host_composition::session_execution_roles;
mod managed_input_projection;
mod managed_model_capability;
mod managed_outcome;
mod mcp_host;
mod memory;
#[cfg(test)]
mod memory_stores;
mod model_content_materializer;
mod no_model;
mod outcome_controller;
mod outcome_report;
mod redact;
mod run_application_host;
mod run_assembly;
pub use run_assembly::RunAssembly;
mod run_exec;
mod session_memory_tools;
mod session_tools;
mod skill_catalog;
mod skills;
mod step_projection;
mod store;
mod thread_reads;
pub use thread_reads::ThreadReads;

#[cfg(test)]
use awaken_session_mcp::test_mcp;
mod tool_output_spill;
mod unavailable_worker;
mod uninstalled_environment;
mod web_search;
mod worker_services;

pub(crate) use awaken_session_realization_contract::ResourceOperationFence;

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_application_contract::{DelegatedRun, Pending, StepOutcome};
use awaken_session_configuration_contract::{AgentCapabilities, BuiltinTool, CustomTool};
use awaken_session_execution_contract::{OutcomeDrive, SessionRuntime, ToolPermissionDecision};
use awaken_session_types::RunError;

#[cfg(any(test, feature = "test-support"))]
pub use crate::authority::EphemeralRuntimeAuthority;
pub(crate) use crate::authority::{LocalAuthority, SessionAuthority};
pub use crate::authority::{
    LocalCommit, LocalCommitAdapter, LocalCommitQueries, RuntimeAuthority, RuntimeAuthorityError,
    UpstreamAuthority,
};
pub(crate) use crate::host::HostOutcomeIteration;
pub use crate::host::{
    CommittedStepReceipt, HostError, HostErrorKind, HostOutcomeDrive, HostOutcomeReport,
};
pub(crate) use crate::installed_session::InstalledSessions;
// The neutral session substrate and its resume vocabulary.
pub use crate::acp_capability_probe::SessionAcpCapabilityNegotiator;
pub use crate::cache_volume::{
    CacheVolumeInitializer, CacheVolumeWarmup, SandboxCacheVolumeInitializer,
};
pub use crate::commit_ingest::session_commit_owner;
pub use crate::config::{
    advertised_tools, authorable_tools, block_text, platform_plugin_capabilities,
    platform_plugin_capabilities_with_web_search,
};
pub use crate::deployment_config::{
    AcpWorkerProfile, ContentCaptureSettings, ContentRedaction, DeploymentConfig, DispatchBackend,
    PackageImageBuilder, SandboxPackageSettingsInput, SandboxSettings, SandboxTier, StoreKind,
    Wake, default_postgres_max_connections, session_sandbox_base,
};
pub(crate) use crate::host::ApplicationPortBinding;
pub use crate::host::capability::WorkerContentAdapters;
#[cfg(any(test, feature = "test-support"))]
pub use crate::host::capability::{LocalResourcePorts, LocalSkillPorts, test_memory_content};
pub use crate::host::{
    AttemptExecutorDecorator, DispatchPoolSizing, ExecutionTopology, HostDependencies, HostProcess,
    HostResume, LeaseOwnerId, LocalExecution, QueueDepthSource, RemoteAttemptInstallation,
    RunIngress, SessionApplicationBindingError, SessionApplicationBindings,
    SessionExecutionDependencies, SharedHost, TrustedHostPolicy, WarmPool,
};
pub use crate::hub::{ThreadEvent, ThreadEventHub};
pub use crate::no_model::{NoModelConfiguredExecutor, UNCONFIGURED_MODEL_REF};
pub use crate::run_application_host::RunApplicationHost;
use awaken_credential_materializer::{CredentialRefreshFactory, PinnedCredentialMaterializer};
use awaken_resource_contract::FileContentSource;
#[cfg(test)]
use awaken_resource_contract::RepositoryBindingVerifier;
use managed_adapter_error::to_run_error;
pub(crate) use managed_input_projection::{session_system_message, user_message};
// The model-route seam (R1/R2/R5): a process startup supplies its own
// `InferenceExecutorMaterializer` to map a session's model ref to a labeled executor.
// ── Managed Agents adapter over the shared host ─────────────────────────────

/// Map a neutral terminal state to the Managed idle `stop_reason`. `RequiresAction`
/// carries no event ids here; the projection refills them from the pending tool.
/// The Managed Agents `SessionRuntime` port implemented over the shared host.
/// Holds only an `Arc<SharedHost>` plus runtime-side materialization SPIs, so it
/// configures with any other adapter bound to the same host.
#[derive(Clone)]
pub struct ManagedHost {
    host: Arc<SharedHost>,
}

/// Composition failure for the one process-local Session coordination port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AgentCoordinationInstallError {
    #[error("Session Agent coordination application is already installed")]
    AlreadyInstalled,
    #[error("Session dispatch observation source is unavailable")]
    DispatchObservationUnavailable,
    #[error("Session dispatch observation source differs from the canonical Runtime")]
    DispatchObservationMismatch,
    #[error("Session coordination application is unavailable at composition")]
    ApplicationUnavailable,
}

#[async_trait::async_trait]
impl SessionRuntime for ManagedHost {
    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        self.host
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        let result = self
            .host
            .run(Some(agent), thread, vec![user_message(content)])
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        self.host
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        let result = self
            .host
            .resume(thread, tool_use_id, HostResume::Permission(decision))
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        self.host
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        let result = self
            .host
            .resume(
                thread,
                tool_use_id,
                HostResume::ClientResult { content, is_error },
            )
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    fn model(&self) -> String {
        self.host.run_assembly.model()
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for ManagedHost {
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<awaken_session_execution_contract::SessionRunReservation, RunError> {
        let session_id = command.session_id.clone();
        if command.session_id.as_str().trim().is_empty()
            || command.agent_id.trim().is_empty()
            || command.operation_id.trim().is_empty()
            || command.run_id.0.trim().is_empty()
            || command.messages.is_empty()
        {
            return Err(RunError::bad_request(
                "Session Run reservation is incomplete",
            ));
        }
        // Freeze the Session application's complete immutable command before
        // Skill expansion or any current Agent/model/Resource/placement
        // projection can change the delivery payload reconstructed by a retry.
        let command_fingerprint = command.command_fingerprint();
        let input = command.messages.clone();
        // The Session application's Run reservation enters this host for the
        // Session: its install path, so the Session-dispatch mark has a slot.
        self.host
            .installed_sessions
            .install(command.session_id.as_str());
        self.host
            .realization_executor
            .validate_thread_resource_bindings(command.session_id.as_str())
            .await?;
        let ctx = self
            .host
            .run_assembly
            .ctx_for_session_coordination(command.session_id.as_str(), Some(&command.agent_id))
            .await
            .map_err(to_run_error)?;
        let reservation_context_is_ephemeral = ctx.is_coordination_envelope();
        let reservation = async {
            let input = match &ctx.skill_registry {
                Some(registry) => awaken_ext_skills::expand_slash_commands(
                    registry.as_ref(),
                    command.session_id.as_str(),
                    input,
                )
                .map_err(RunError::internal)?,
                None => input,
            };
            let (_, mut activation) =
                ctx.runtime
                    .prepare(&ctx.config, command.session_id.to_string(), input);
            activation.run_id = command.run_id;
            // Application requirements may only narrow the Session-owned tool
            // authority; they cannot restore a capability removed by the
            // frozen Session profile.
            activation = activation
                .narrow_tools(command.execution_requirements.tool_capability_narrowing)
                .map_err(RunError::bad_request)?;
            activation.model_ref_override = self
                .host
                .agent
                .inference_routing
                .override_for(command.session_id.as_str());
            activation.data_subject_id = command
                .data_subject_id
                .map(awaken_runtime_contract::DataSubjectId);
            let mut request = self
                .host
                .run_assembly
                .prepare_dispatch(activation)
                .with_traceparent(command.traceparent)
                .map_err(|error| to_run_error(error.into()))?
                .with_session_command_fingerprint(command_fingerprint)
                .with_session_run_replacement(command.replacement);
            if !command
                .execution_requirements
                .required_worker_capabilities
                .is_empty()
            {
                // Protocol-specific capabilities are implemented only by a
                // registered Worker; never fall back to a generic local runner.
                let strict_remote =
                    awaken_dispatch_contract::PlacementRequirements::remote_required();
                request.placement.location = strict_remote.location;
                if request.placement.contract_version == 0 {
                    request.placement.contract_version = strict_remote.contract_version;
                    request.placement.dispatch_contract_version =
                        strict_remote.dispatch_contract_version;
                    request.placement.runtime_protocol_version =
                        strict_remote.runtime_protocol_version;
                }
                request
                    .placement
                    .required_capabilities
                    .extend(command.execution_requirements.required_worker_capabilities);
            }
            match self
                .host
                .coordinator_dispatch()
                .map_err(to_run_error)?
                .reserve_session_run(request, 30_000)
                .await
                .map_err(|error| RunError::unavailable(error.to_string()))?
            {
                awaken_dispatch_contract::SessionRunReservationOutcome::Reserved => {
                    Ok(awaken_session_execution_contract::SessionRunReservation::Reserved)
                }
                awaken_dispatch_contract::SessionRunReservationOutcome::AlreadyReserved => {
                    Ok(awaken_session_execution_contract::SessionRunReservation::AlreadyReserved)
                }
                awaken_dispatch_contract::SessionRunReservationOutcome::RecoveryClaimed => {
                    Ok(awaken_session_execution_contract::SessionRunReservation::RecoveryClaimed)
                }
                awaken_dispatch_contract::SessionRunReservationOutcome::AlreadyActivated {
                    session_activity_epoch,
                } => Ok(
                    awaken_session_execution_contract::SessionRunReservation::AlreadyActivated {
                        session_activity_epoch,
                    },
                ),
                awaken_dispatch_contract::SessionRunReservationOutcome::Completed => {
                    Ok(awaken_session_execution_contract::SessionRunReservation::Completed)
                }
                awaken_dispatch_contract::SessionRunReservationOutcome::Conflict => Err(
                    RunError::bad_request("Session Run id was reused with different input"),
                ),
            }
        }
        .await;
        if reservation_context_is_ephemeral {
            self.host
                .realization_executor
                .environments()
                .evict_session_for_rebuild(session_id.as_str())
                .await?;
        }
        reservation
    }

    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, RunError> {
        self.host
            .activate_session_run_reservation(delivery)
            .await
            .map_err(to_run_error)
    }

    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let receipt = self
            .host
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
            .map_err(to_run_error)?;
        crate::step_projection::settled_step(receipt)
    }

    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<awaken_session_execution_contract::SessionAgentMessageReceipt, RunError> {
        self.host
            .admit_coordinated_run(command)
            .await
            .map_err(to_run_error)
    }

    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        RunError,
    > {
        if thread_id == session_id {
            return Ok(Some(self.host.hub.live_subscription(thread_id)));
        }
        let links = self
            .host
            .thread_reads
            .coordinated_threads(session_id.as_str(), None)
            .await
            .map_err(to_run_error)?;
        if !links
            .iter()
            .any(|link| link.session_id == session_id && link.thread_id.0.as_str() == thread_id)
        {
            return Err(RunError::bad_request(
                "live observer target is not a coordinated Session Thread",
            ));
        }
        Ok(Some(self.host.hub.live_subscription(thread_id)))
    }

    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        self.host
            .continue_session_agent_report(command)
            .await
            .map_err(to_run_error)
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), RunError> {
        self.host
            .interrupt_session_thread(session_id.as_str(), child_thread_id)
            .await
            .map_err(to_run_error)
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        self.host
            .reply_session_thread_tool(delivery)
            .await
            .map_err(to_run_error)
    }

    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let receipt = self
            .host
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
            .map_err(to_run_error)?;
        crate::step_projection::settled_step(receipt)
    }

    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), RunError> {
        self.host
            .archive_session_thread(
                session_id.as_str(),
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            )
            .await
            .map_err(to_run_error)
    }

    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, RunError> {
        self.host
            .quiesce_terminal_delegations(thread)
            .await
            .map_err(to_run_error)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for ManagedHost {
    async fn source_dispatch_observation(
        &self,
        session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionSourceDispatchObservation {
        let Ok(source) = self.host.coordinator_dispatch() else {
            return Default::default();
        };
        awaken_dispatch_execution::read_source_dispatch_observation(source.as_ref(), session).await
    }

    async fn execution_recovery_observation(
        &self,
        session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionExecutionRecoveryObservation {
        let Ok(source) = self.host.coordinator_dispatch() else {
            return awaken_session_execution_contract::SessionExecutionRecoveryObservation::NotProven {
                reason: awaken_session_execution_contract::SessionExecutionRecoveryGap::HistoryUnavailable,
            };
        };
        let projection = awaken_session_execution_contract::CommittedSessionProjection::new(
            self.host.thread_reads.clone(),
            Arc::new(self.clone()),
        );
        awaken_dispatch_execution::read_execution_recovery(source.as_ref(), &projection, session)
            .await
    }

    async fn coordinated_admission_receipt(
        &self,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<Option<awaken_session_execution_contract::SessionAgentMessageReceipt>, RunError> {
        use awaken_dispatch_contract::SessionRunAdmissionObservation;
        if command.activity_epoch() == Some(0)
            || matches!(command, awaken_session_execution_contract::CoordinatedAdmissionRef::Report(_, baseline)
                if baseline.agent_revision.is_none_or(|revision| revision == 0))
        {
            return Err(RunError::unavailable(
                "original coordination activity epoch is invalid",
            ));
        }
        let run = command.run_id();
        let source = self
            .host
            .coordinator_dispatch()
            .map_err(|error| RunError::unavailable(error.message))?;
        let reads: &dyn awaken_dispatch_contract::SessionRunAdmissionReads = source.as_ref();
        let observed = reads
            .observe_session_run_admission(&run)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        match observed {
            SessionRunAdmissionObservation::Missing => Ok(None),
            SessionRunAdmissionObservation::Live(request)
                if request.identity_scope
                    == awaken_dispatch_contract::DispatchIdentityScope::FullDispatch
                    && request.session_command_fingerprint.is_none()
                    && request.session_thread_id.as_ref()
                        == Some(&awaken_agent_contract::agent::thread::Id(
                            command.session_id().to_string(),
                        ))
                    && command.activity_epoch().is_some_and(|epoch| epoch != 0)
                    && request.session_activity_epoch == command.activity_epoch()
                    && awaken_dispatch_execution::coordinated_activation_matches(
                        &command,
                        &request.activation,
                    ) =>
            {
                Ok(Some(
                    awaken_session_execution_contract::SessionAgentMessageReceipt {
                        thread_id: command.thread_id(),
                    },
                ))
            }
            _ => Err(RunError::unavailable(
                "original coordinated admission evidence is incomplete or mismatched",
            )),
        }
    }

    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
        let commit = self
            .host
            .thread_reads
            .commit_for_read(session_id.as_str())
            .await
            .map_err(to_run_error)?;
        commit
            .authoritative_run(run_id)
            .await
            .map(|record| record.map(|record| record.state))
            .map_err(|error| RunError::unavailable(error.to_string()))
    }

    async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.host
            .thread_reads
            .delegated_runs(thread)
            .await
            .map_err(to_run_error)
    }

    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<awaken_session_execution_contract::CoordinatedThreadLink>, RunError> {
        self.host
            .thread_reads
            .coordinated_threads(session_id.as_str(), observed_root)
            .await
            .map_err(to_run_error)
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_execution_contract::SessionThreadToolReplyFence, RunError> {
        self.host
            .session_thread_tool_reply_fence(command)
            .await
            .map_err(to_run_error)
    }

    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<awaken_session_execution_contract::SessionAgentMessageCommand, RunError> {
        self.host
            .validate_session_agent_message_source(command)
            .await
            .map_err(to_run_error)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::BudgetRuntime for ManagedHost {
    async fn resume_budget_reached(
        &self,
        delivery: awaken_session_application_contract::SessionBudgetResumeDelivery,
    ) -> Result<awaken_session_application_contract::SessionBudgetResumeDisposition, RunError> {
        self.host
            .resume_budget_reached(delivery)
            .await
            .map_err(to_run_error)
    }

    async fn session_budget_resume_tickets(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<Vec<awaken_session_application_contract::SessionBudgetResumeTicket>, RunError> {
        self.host
            .session_budget_resume_tickets(session_id.as_str())
            .await
            .map_err(to_run_error)
    }

    async fn session_usage(
        &self,
        thread: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        self.host
            .thread_reads
            .session_thread_usage(
                thread,
                &awaken_agent_contract::agent::thread::Id(thread.to_string()),
            )
            .await
            .map_err(to_run_error)
    }

    async fn session_thread_usage(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        self.host
            .thread_reads
            .session_thread_usage(
                session_id.as_str(),
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            )
            .await
            .map_err(to_run_error)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRunRuntime for ManagedHost {
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
    ) -> Result<StepOutcome, RunError> {
        self.host
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        let result = self
            .host
            .run_attributed(
                Some(agent),
                thread,
                vec![user_message(content)],
                data_subject_id.map(awaken_runtime_contract::DataSubjectId),
            )
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.host
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        // Same committed Run as `run`; `sink` mirrors in-flight `stream::Kind` so
        // the Managed adapter can project live `agent.message` previews.
        let result = self
            .host
            .run_streaming(Some(agent), thread, vec![user_message(content)], sink)
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.host
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        let result = self
            .host
            .run_streaming_attributed(
                Some(agent),
                thread,
                vec![user_message(content)],
                sink,
                data_subject_id.map(awaken_runtime_contract::DataSubjectId),
            )
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.host.interrupt(thread).await.map_err(to_run_error)
    }

    /// Replace only the already-selected model projection for one Session and
    /// force its next context construction to consume that exact selection.
    async fn rebind_model(&self, thread: &str, model: &str) -> Result<(), RunError> {
        // R5: re-stage the thread's model and evict its cached context so the next
        // Run rebuilds with the newly resolved executor (native switch is O(1); an
        // ACP thread's cached context relaunches its CLI on rebuild).
        self.host.run_assembly.register_thread_model(thread, model);
        self.host
            .realization_executor
            .environments()
            .evict_session_for_rebuild(thread)
            .await?;
        Ok(())
    }

    async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        managed_model_capability::supports_mid_conversation_system(
            &self.host.run_assembly.model_for_thread(thread),
        )
    }

    async fn pending_tool(&self, thread: &str) -> Result<Option<Pending>, RunError> {
        self.host
            .thread_reads
            .pending_tool(thread)
            .await
            .map_err(to_run_error)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for ManagedHost {
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, RunError> {
        managed_outcome::prepare(
            &self.host,
            thread,
            outcome_id,
            description,
            rubric,
            max_iterations,
        )
        .await
    }

    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError> {
        managed_outcome::define(&self.host, thread, description, rubric, max_iterations).await
    }

    async fn continue_outcome(&self, thread: &str) -> Result<Option<OutcomeDrive>, RunError> {
        managed_outcome::resume(&self.host, thread).await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for ManagedHost {
    /// Advertise the host's provisioned surface on the created session: its built-in
    /// tools (folded into the agent toolset by the adapter), client tools, offered
    /// skills, and delegate roster. (MCP servers and file resources are not advertised
    /// — the local host wires no MCP capability and has no Files-API resource yet.)
    fn capabilities(&self) -> AgentCapabilities {
        self.capabilities_for_workspace(self.host.run_assembly.local_workspace())
    }

    fn capabilities_for(&self, thread: &str) -> AgentCapabilities {
        let workspace = self.host.run_assembly.thread_workspace(thread);
        let mut capabilities = self.capabilities_for_workspace(&workspace);
        if let Some(delegate_ids) = self.host.run_assembly.thread_delegate_ids(thread) {
            capabilities.delegates = delegate_ids;
        }
        capabilities
    }
}

use awaken_dispatch_contract::SessionRunReservations as _;

pub(crate) use awaken_session_configuration_contract::mcp as session_mcp;

pub(crate) use awaken_session_configuration_contract::resource as session_resource;

#[cfg(test)]
use awaken_session_realization::environment_owner;
use awaken_session_realization::{
    artifact_harvest, cache_volume, environment_holding, environment_providers,
    execution_authority, frozen_inputs, managed_input_layout, memory_binding, realization_error,
    realization_executor, realization_state, resource_staging, sandbox_projection,
    session_environment, session_publication, session_realizations, skill_delivery,
    staged_resources, terminal_effects,
};
#[cfg(test)]
#[path = "realization_composed_tests/mod.rs"]
mod realization_composed_tests;
