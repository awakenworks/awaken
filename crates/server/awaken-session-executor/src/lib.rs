// @formal entrypoint session.cleanup_completion_requires_exact_identity_and_canonical_artifact_evidence
//! `awaken-session-executor` — authority-free shared execution host infrastructure.
//!
//! It owns protocol-neutral execution coordination, Session-environment
//! realization, capability assembly, and the [`SharedHost`] used by every
//! driving protocol. [`ManagedHost`] and [`RunApplicationHost`] adapt Managed
//! Agents and AI SDK / AG-UI / A2A onto that one host, so they cannot create
//! protocol-specific run, resume, or terminal-state implementations.
//!
//! This crate consumes capabilities selected by process startup. It never opens
//! or selects a SQL/filesystem authority Store, never owns a public protocol or
//! domain aggregate, and never decides the deployment topology. Coordinator
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
mod bound_environment;
mod cache_volume;
mod capabilities;
mod commit_ingest;
mod compact;
mod config;
mod coordination;
mod delegate;
mod deployment_config;
mod dispatch_session_runtime;
mod durable_operations;
mod environment_capacity;
mod environment_continuation;
mod environment_effects;
mod environment_holding;
mod environment_owner;
mod environment_providers;
mod frozen_inputs;
mod host;
mod host_error;
mod hub;
mod inference_routing;
mod installed_session;
mod judge;
mod lazy_sandbox;
mod live_inbox;
mod managed_adapter_error;
mod managed_host_composition;
mod managed_input_layout;
mod managed_input_projection;
mod managed_model_capability;
mod managed_outcome;
mod managed_resource_projection;
mod mcp_host;
mod memory;
mod memory_binding;
mod memory_stores;
mod model_content_materializer;
mod no_model;
mod outcome_controller;
mod provisioning;
mod realization_executor;
mod redact;
mod resource_staging;
mod run_application_host;
mod run_exec;
mod session_environment;
mod session_memory_tools;
mod session_publication;
mod session_realizations;
mod session_tools;
mod skill_catalog;
mod skill_delivery;
mod skills;
mod staged_resources;
mod step_projection;
mod store;
mod terminal_cleanup;
mod terminal_repository_publication;
#[cfg(test)]
use awaken_session_mcp::test_mcp;
use awaken_session_execution_contract::EnvironmentRuntime as _;
mod tool_output_spill;
mod unavailable_worker;
mod uninstalled_environment;
mod web_search;
mod worker_services;

use crate::skill_delivery::skill_store_run_error;
pub(crate) use awaken_session_realization_contract::ResourceOperationFence;
pub(crate) use dispatch_session_runtime::DispatchSessionRuntime;

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_runtime_contract::live_inbox::{LiveInboxMessageId, MessageOrigin, Offer};
use awaken_session_application_contract::{DelegatedRun, Pending, StepOutcome};
use awaken_session_configuration_contract::{AgentCapabilities, BuiltinTool, CustomTool};
use awaken_session_execution_contract::{
    LiveInboxEntry, LiveInboxError, LiveInboxSnapshot, OutcomeDrive, SessionRuntime,
    ToolPermissionDecision,
};
use awaken_session_types::RunError;

#[cfg(any(test, feature = "test-support"))]
pub use crate::authority::EphemeralRuntimeAuthority;
pub(crate) use crate::authority::{LocalAuthority, SessionAuthority};
pub use crate::authority::{
    LocalCommit, LocalCommitAdapter, LocalCommitQueries, RuntimeAuthority, RuntimeAuthorityError,
    UpstreamAuthority,
};
pub use crate::host::{
    CommittedStepReceipt, HostError, HostErrorKind, HostOutcomeDrive, HostOutcomeReport,
};
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
#[cfg(any(test, feature = "test-support"))]
pub use crate::host::capability::{LocalResourcePorts, LocalSkillPorts};
pub use crate::host::{
    AttemptExecutorDecorator, DispatchPoolSizing, ExecutionTopology, HostDependencies, HostProcess,
    HostResume, LeaseOwnerId, LocalExecution, QueueDepthSource, RemoteAttemptInstallation,
    RunIngress, SessionApplicationBindingError, SessionApplicationBindings,
    SessionExecutionDependencies, SharedHost, TrustedHostPolicy, WarmPool, remote_worker_placement,
    self_hosted_inference_holder,
};
pub use crate::hub::{ThreadEvent, ThreadEventHub};
pub use crate::no_model::{NoModelConfiguredExecutor, UNCONFIGURED_MODEL_REF};
pub use crate::realization_executor::RealizationExecutor;
pub use crate::run_application_host::RunApplicationHost;
use awaken_credential_materializer::{CredentialRefreshFactory, PinnedCredentialMaterializer};
use awaken_resource_contract::FileContentSource;
#[cfg(test)]
use awaken_resource_contract::RepositoryBindingVerifier;
use managed_adapter_error::{to_live_inbox_error, to_run_error};
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
            .realizer
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
            .realizer
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
            .realizer
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
        self.host.model()
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for ManagedHost {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_lease_contract::authority::AuthorityProofStarted, RunError> {
        awaken_authority_clock::authority_proof_started()
            .map_err(|error| RunError::unavailable(format!("authority clock: {error}")))
    }

    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, RunError> {
        self.host
            .renew_session_realization_projections(
                Some(control),
                Some((owner, runtime_incarnation)),
                timing,
            )
            .await
            .map_err(to_run_error)
    }

    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), RunError> {
        self.host
            .realizer
            .validate_prospective_session_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        let realizations = self.host.installed_sessions.realizations();
        // Every Environment-owner projection, including lease-only recovery,
        // crosses the same lifecycle guard as physical publication/retirement.
        let _lifecycle = realizations.lock_lifecycle(thread).await;
        if mode.prepares_session() {
            // Cause/effect rule P2: an unprotected active Runtime rejects before
            // any frozen coordinate is published. The same lifecycle mutex also
            // prevents a peer from observing installed facts before the Managed
            // execution marker and deferred executor are complete.
            let preparation_needed = self.session_preparation_needed(thread)?;
            self.install_projection_facts(thread, &projection, &mode)
                .await?;
            if preparation_needed {
                self.complete_session_preparation(thread, &projection.baseline.environment);
            }
        } else {
            self.install_projection_facts(thread, &projection, &mode)
                .await?;
        }
        drop(_lifecycle);
        if mode.adopts_resident_environment()
            && let Some(binding) = projection.environment.binding()
        {
            self.adopt_session_environment(&projection.baseline.agent_id, thread, binding)
                .await?;
        }
        Ok(())
    }

    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        let realizations = self.host.installed_sessions.realizations();
        let missing = || {
            RunError::classified(
                "session_realization_lease_missing",
                "cannot renew a Session realization lease that is not installed",
            )
        };
        self.host.realizer.execution
            .authorize_worker_effect(|| {
                Ok(realizations
            .modify_realization(thread, |r| {
                let current = r.authority.lease().cloned().ok_or_else(missing)?;
                if !r.renew_lease(&current, lease) {
                    return Err(RunError::classified(
                        "session_realization_lease_conflict",
                        "Session realization renewal is not a monotonic successor of the installed fence",
                    ));
                }
                Ok(())
            })
            .unwrap_or_else(|| Err(missing())))
            })
            .map_err(|error| {
                RunError::classified("worker_execution_authority_denied", error.to_string())
            })?
    }

    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<(), RunError> {
        self.host
            .invalidate_session_realization_lease(thread, lease);
        Ok(())
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
        self.host
            .realizer
            .validate_thread_resource_bindings(command.session_id.as_str())
            .await?;
        let ctx = self
            .host
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
                .resolved_dispatch_with_traceparent(activation, command.traceparent)
                .map_err(to_run_error)?
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
                .realizer
                .evict_session_for_rebuild(session_id.as_str())
                .await;
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
    async fn execution_recovery_observation(
        &self,
        session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionExecutionRecoveryObservation {
        let Ok(source) = self.host.coordinator_dispatch() else {
            return awaken_session_execution_contract::SessionExecutionRecoveryObservation::NotProven {
                reason: awaken_session_execution_contract::SessionExecutionRecoveryGap::HistoryUnavailable,
            };
        };
        awaken_dispatch_execution::read_execution_recovery(source.as_ref(), self, session).await
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
        self.host.delegated_runs(thread).await.map_err(to_run_error)
    }

    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<awaken_session_execution_contract::CoordinatedThreadLink>, RunError> {
        self.host
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
            .realizer
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
            .realizer
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
            .realizer
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
        self.host.register_thread_model(thread, model);
        self.host.realizer.evict_session_for_rebuild(thread).await;
        Ok(())
    }

    async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        managed_model_capability::supports_mid_conversation_system(
            &self.host.model_for_thread(thread),
        )
    }

    async fn pending_tool(&self, thread: &str) -> Result<Option<Pending>, RunError> {
        self.host.pending_tool(thread).await.map_err(to_run_error)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::EnvironmentRuntime for ManagedHost {
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_configuration_contract::SessionToolConfiguration,
    ) -> Result<(), RunError> {
        self.host
            .installed_sessions
            .replace_frozen_with(thread, |next| next.tools = Some(tools));
        self.host.realizer.evict_session_for_rebuild(thread).await;
        Ok(())
    }

    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError> {
        // Managed Session restoration is a continuity contract: the durable
        // binding must decode, belong to this Session/provider, and still name a
        // ready physical environment. Run-dispatch recovery has its own explicit
        // RebuildFromCommittedTruth policy; applying that fallback here would
        // turn corrupt or deleted Session authority into a replacement sandbox.
        let provider = self
            .host
            .realizer
            .projected_session_environment_provider(thread, Some(agent))
            .map_err(to_run_error)?;
        let disposition = self
            .host
            .adopt_bound_session_environment(thread, Some(binding), provider, None, false)
            .await
            .map_err(to_run_error)?;
        if disposition != crate::session_environment::SessionEnvironmentAdoptionDisposition::Ready {
            return Err(RunError::internal(
                "durable Session Environment adoption did not publish a ready binding",
            ));
        }
        self.host
            .ctx_for(thread, Some(agent))
            .await
            .map_err(to_run_error)?;
        Ok(())
    }

    async fn quiesce_session_environment(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_state_contract::SandboxGeneration,
        expected_mcp_generations: &[crate::session_mcp::McpGenerationRef],
    ) -> Result<awaken_session_state_contract::QuiescenceReceipt, RunError> {
        self.quiesce_environment_continuation(
            thread,
            operation,
            source_effect_id,
            source_binding,
            generation,
            expected_mcp_generations,
        )
        .await
    }

    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_state_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_state_contract::CheckpointReceipt, RunError> {
        self.host
            .realizer
            .checkpoint_environment_continuation(thread, request)
            .await
    }

    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &awaken_session_state_contract::SourceReleasePreparationEffect,
        generation: &awaken_session_state_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_state_contract::SourceReleasePreparedReceipt, RunError> {
        self.host
            .realizer
            .prepare_environment_continuation_source_release(
                thread,
                preparation,
                generation,
                source_binding,
            )
            .await
    }

    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &awaken_session_state_contract::SourceReleaseDisposal,
    ) -> Result<awaken_session_state_contract::SourceDisposedReceipt, RunError> {
        self.host
            .realizer
            .dispose_prepared_environment_continuation_source(thread, disposal)
            .await
    }

    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_state_contract::SandboxRestoreRequest,
    ) -> Result<awaken_session_state_contract::RestoreReceipt, RunError> {
        self.host
            .realizer
            .restore_environment_continuation(request)
            .await
    }

    async fn delete_session_checkpoint(
        &self,
        _thread: &str,
        checkpoint: &awaken_session_state_contract::SandboxCheckpointRef,
    ) -> Result<(), RunError> {
        self.host
            .realizer
            .delete_environment_continuation_checkpoint(checkpoint)
            .await
    }

    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<crate::session_resource::ResolvedSkillBinding>, RunError> {
        if skills.is_empty() {
            return Ok(Vec::new());
        }
        self.host
            .tools
            .skills
            .resolve(workspace_id, skills)
            .await
            .map_err(skill_store_run_error)
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        transition: &crate::session_resource::SessionResourceTransition,
    ) -> Result<(), RunError> {
        self.host
            .realizer
            .apply_session_inputs_with_context(thread, transition, None, None)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for ManagedHost {
    async fn owns_thread(&self, thread: &str) -> Result<bool, RunError> {
        self.host
            .has_durable_thread(thread)
            .await
            .map_err(to_run_error)
    }

    /// Committed transcript from durable truth, so the adapter can rehydrate a
    /// session lost to a process restart and resume its awaiting run (ADR-0039).
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_agent_contract::Message>, RunError> {
        self.host
            .committed_messages(thread)
            .await
            .map_err(to_run_error)
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.host
            .session_thread_recovery_snapshot(
                session_id.as_str(),
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            )
            .await
            .map_err(to_run_error)
    }

    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        let snapshot = self
            .host
            .session_thread_run_recovery_snapshot(
                session_id.as_str(),
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                run_id,
            )
            .await
            .map_err(to_run_error)?;
        awaken_session_execution_contract::select_recovery_snapshot_run(Some(snapshot), run_id)
    }

    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, RunError> {
        self.host
            .session_thread_disposition(
                session_id.as_str(),
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            )
            .await
            .map_err(to_run_error)
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

    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<awaken_session_execution_contract::CommittedOutcomeProjection>, RunError> {
        managed_outcome::committed_projection(&self.host, thread, outcome_id).await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for ManagedHost {
    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<awaken_session_state_contract::SessionRepositoryPublicationEffect, RunError> {
        self.host
            .realizer
            .publish_terminal_repository(command, lease)
            .await
    }

    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        self.host
            .prepare_terminal_cleanup_effect(effect, authorization)
            .await
            .map_err(to_run_error)
    }

    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        self.host
            .acknowledge_terminal_cleanup_preparation(effect)
            .await;
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_state_contract::SessionCleanupDisposalReceipt, RunError> {
        self.host
            .dispose_terminal_cleanup_effect(effect)
            .await
            .map_err(to_run_error)
    }

    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        self.host
            .acknowledge_terminal_cleanup_disposal(effect)
            .await;
    }

    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &crate::session_resource::SessionRealizationLease,
    ) {
        self.host
            .acknowledge_completed_terminal_cleanup(session_id.as_str(), lease)
            .await;
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        self.host
            .install_terminal_cleanup_projection(assignment, proof)
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
impl awaken_session_execution_contract::LiveInboxRuntime for ManagedHost {
    async fn live_inbox_snapshot(&self, thread: &str) -> LiveInboxSnapshot {
        match self.host.live_inbox(thread).await {
            Some(inbox) => LiveInboxSnapshot {
                active: true,
                version: inbox.version(),
                messages: inbox
                    .list()
                    .into_iter()
                    .map(|entry| LiveInboxEntry {
                        id: entry.id.0,
                        content: entry.message.content,
                    })
                    .collect(),
            },
            None => LiveInboxSnapshot::inactive(),
        }
    }

    async fn live_inbox_queue(
        &self,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxError> {
        let inbox = self
            .host
            .live_inbox(thread)
            .await
            .ok_or(LiveInboxError::Inactive)?;
        // Queued through the wire surface — an out-of-band injection into a live
        // run, so it is tagged External (what a product maps operator steering onto).
        match inbox.offer_as(MessageOrigin::External, user_message(content)) {
            Offer::Accepted(id) => Ok(id.0),
            // The attempt closed between lookup and offer, or its finite live
            // identity space was exhausted: both route like no active inbox.
            Offer::Closed => Err(LiveInboxError::Inactive),
        }
    }

    async fn live_inbox_remove(&self, thread: &str, id: u64) -> Result<(), LiveInboxError> {
        let inbox = self
            .host
            .live_inbox(thread)
            .await
            .ok_or(LiveInboxError::Inactive)?;
        inbox
            .remove(LiveInboxMessageId(id))
            .map(|_| ())
            .map_err(to_live_inbox_error)
    }

    async fn live_inbox_replace(
        &self,
        thread: &str,
        id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxError> {
        let inbox = self
            .host
            .live_inbox(thread)
            .await
            .ok_or(LiveInboxError::Inactive)?;
        inbox
            .replace(LiveInboxMessageId(id), user_message(content))
            .map_err(to_live_inbox_error)
    }

    async fn live_inbox_reorder(
        &self,
        thread: &str,
        order: Vec<u64>,
    ) -> Result<(), LiveInboxError> {
        let inbox = self
            .host
            .live_inbox(thread)
            .await
            .ok_or(LiveInboxError::Inactive)?;
        let order: Vec<LiveInboxMessageId> = order.into_iter().map(LiveInboxMessageId).collect();
        inbox.reorder(&order).map_err(to_live_inbox_error)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for ManagedHost {
    /// Advertise the host's provisioned surface on the created session: its built-in
    /// tools (folded into the agent toolset by the adapter), client tools, offered
    /// skills, and delegate roster. (MCP servers and file resources are not advertised
    /// — the local host wires no MCP capability and has no Files-API resource yet.)
    fn capabilities(&self) -> AgentCapabilities {
        self.capabilities_for_workspace(self.host.local_workspace())
    }

    fn capabilities_for(&self, thread: &str) -> AgentCapabilities {
        let workspace = self.host.thread_workspace(thread);
        let mut capabilities = self.capabilities_for_workspace(&workspace);
        if let Some(delegate_ids) = self.host.thread_delegate_ids(thread) {
            capabilities.delegates = delegate_ids;
        }
        capabilities
    }
}

// Keep the public trait implementation textually at the crate root so this
// physical responsibility split cannot change its rustc DefPath.
include!("managed_host_mcp_attachment.rs");

use awaken_dispatch_contract::SessionRunReservations as _;

pub(crate) use awaken_session_configuration_contract::mcp as session_mcp;

pub(crate) use awaken_session_configuration_contract::resource as session_resource;
