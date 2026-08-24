//! `awaken-runtime-host` — authority-free shared execution host infrastructure.
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
//! injects the sole durable [`RuntimeAuthority`]; a database-less Worker injects
//! claim-fenced transports. Product composition mounts the corresponding
//! routers around these protocol-neutral behaviors.

mod acp_backend;
mod acp_capability_probe;
mod acp_provision;
mod acp_serve;
mod acp_tool_export;
mod agent_catalog;
mod agent_runner;
mod application;
mod authority;
mod background;
mod cache_volume;
mod capabilities;
mod commit_ingest;
mod compact;
mod config;
mod container_environment;
mod coordination;
pub use container_environment::{
    ContainerEnvironmentComponents, build_container_environment, package_image_provisioner,
};
mod delegate;
mod deployment_config;
mod dispatch_session_runtime;
mod durable_operations;
mod environment_continuation;
mod host;
mod hub;
mod inference_routing;
mod judge;
mod lazy_sandbox;
mod live_inbox;
mod managed_adapter_error;
mod managed_model_capability;
mod managed_outcome;
mod managed_resource_projection;
mod mcp;
mod mcp_relay;
mod memory;
mod memory_stores;
mod model_content_materializer;
mod no_model;
mod outcome_controller;
mod provisioning;
mod redact;
mod run_application_host;
mod run_exec;
mod sandbox_source;
mod session_environment;
mod session_memory_tools;
mod session_slot;
mod session_tools;
pub use session_environment::HandExecutorFactory;
mod skill_catalog;
mod skills;
mod step_projection;
mod store;
#[cfg(test)]
mod test_mcp;
mod tool_output_spill;
mod unavailable_worker;
mod web_search;
mod worker_services;

use crate::session_environment::AgentSandbox as _;
pub(crate) use dispatch_session_runtime::DispatchSessionRuntime;

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_runtime_contract::live_inbox::{LiveInboxMessageId, MessageOrigin, Offer};
use awaken_session_contract::{
    AgentCapabilities, BuiltinTool, CustomTool, DelegatedRun, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, OutcomeDrive, Pending, RunError, SessionRuntime, StepOutcome,
    ToolPermissionDecision,
};

#[cfg(any(test, feature = "test-support"))]
pub use crate::authority::EphemeralRuntimeAuthority;
pub use crate::authority::{
    LocalCommit, LocalCommitAdapter, LocalCommitQueries, RuntimeAuthority, RuntimeAuthorityError,
};
pub use crate::host::{
    CommittedStepReceipt, HostError, HostErrorKind, HostOutcomeDrive, HostOutcomeReport,
};
// The neutral session substrate and its resume vocabulary.
pub use crate::acp_capability_probe::SessionAcpCapabilityNegotiator;
pub use crate::acp_tool_export::{AcpToolExport, AcpToolExporter};
pub use crate::cache_volume::{CacheVolumeInitializer, CacheVolumeWarmup};
pub use crate::host::{
    AttemptExecutorDecorator, HostResume, RemoteAttemptInstallation, SharedHost,
    remote_worker_placement, self_hosted_inference_holder,
};
pub use crate::no_model::{NoModelConfiguredExecutor, UNCONFIGURED_MODEL_REF};
pub use crate::run_application_host::RunApplicationHost;
use awaken_credential_materializer::{CredentialRefreshFactory, PinnedCredentialMaterializer};
use awaken_resource_contract::{FileContentSource, RepositoryBindingVerifier};
// ACP launch projection consumes the Session environment selected by the host.
pub use crate::hub::{ThreadEvent, ThreadEventHub};
pub use crate::redact::PiiRedactor;
pub use crate::sandbox_source::{AcpLaunchRegistry, LaunchSource, resolve_sandbox_tier};
use crate::skill_catalog::skill_store_run_error;
pub use crate::skills::SkillForkPlacement;
use managed_adapter_error::{to_live_inbox_error, to_run_error};
// The config data plane (ADR-0036/slice A): the service + its router + the
// advertised-tools helper the process startup builds a config host from.
pub use crate::acp_provision::PublishedAcpLaunchResolver;
pub use crate::acp_serve::{AcpServeHost, AcpStop, AcpTurn};
pub use crate::commit_ingest::claimed_commit_service;
pub use crate::config::{
    advertised_tools, authorable_config_sections, authorable_config_sections_with_web_search,
    authorable_tools, block_text, platform_plugin_capabilities,
    platform_plugin_capabilities_with_web_search,
};
pub use crate::deployment_config::{
    AcpWorkerProfile, ContainerHandResidency, ContentCaptureSettings, ContentRedaction,
    DeploymentConfig, DispatchBackend, PackageImageBuilder, SandboxSettings, SandboxTier,
    StoreKind, Wake, default_postgres_max_connections,
};
// The model-route seam (R1/R2/R5): a process startup supplies its own
// `InferenceExecutorMaterializer` to map a session's model ref to a labeled executor.
// The managed-vault OAuth seams (ADR-0043): the transport-level refresher, its
// prepared configuration, and the live MCP credential probe.
pub use crate::mcp::ExtMcpProbe;
// ── Managed Agents adapter over the shared host ─────────────────────────────

/// Mint a fresh user message from plain text (Managed `user.message` content is
/// concatenated to text before it enters the host).
fn user_message(content: Vec<ContentBlock>) -> Message {
    Message::new(
        MessageId(awaken_runtime::fresh_process_id("usr")),
        Role::User,
        content,
    )
}

/// Lower one stable Session System input into its sole durable Message form.
/// Fresh Run admission and same-Run tool reply resume share this constructor so
/// System identity, validation, and Role ordering cannot diverge.
fn session_system_message(
    session_id: &str,
    system: &awaken_session_contract::SessionUserRunSystemInput,
) -> Result<Message, RunError> {
    if session_id.trim().is_empty()
        || system.operation_id.trim().is_empty()
        || system.content.is_empty()
    {
        return Err(RunError::bad_request("Session System input is incomplete"));
    }
    Ok(Message::new(
        MessageId::session_system(session_id, &system.operation_id),
        Role::System,
        system.content.clone(),
    ))
}

/// Lower one Session User command into the exact activation input frozen by the
/// existing dispatch reservation. An accompanying System Message precedes the
/// User Message for model semantics, while their stable operation ids preserve
/// the public batch order independently.
fn session_user_run_messages(
    command: &awaken_session_contract::SessionUserRunCommand,
) -> Result<Vec<Message>, RunError> {
    if command.session_id.trim().is_empty()
        || command.operation_id.trim().is_empty()
        || command.run_id.0.trim().is_empty()
        || command.content.is_empty()
        || command.accompanying_system.as_ref().is_some_and(|system| {
            system.operation_id.trim().is_empty() || system.content.is_empty()
        })
    {
        return Err(RunError::bad_request(
            "Session User Run reservation is incomplete",
        ));
    }
    let mut input = Vec::with_capacity(1 + usize::from(command.accompanying_system.is_some()));
    if let Some(system) = &command.accompanying_system {
        input.push(session_system_message(&command.session_id, system)?);
    }
    input.push(Message::new(
        MessageId::session_event_input(&command.session_id, &command.operation_id),
        Role::User,
        command.content.clone(),
    ));
    Ok(input)
}

/// Select the exact target carried by the admitted MCP realization request.
/// Keeping this identity projection explicit prevents credential materialization
/// from silently rebinding the request to a name/target tuple or another derived
/// lookup key.
#[must_use]
fn exact_credential_realization_target<T>(request_target: &T) -> &T {
    request_target
}

#[cfg(kani)]
#[kani::proof]
fn mcp_credential_realization_preserves_the_request_target_exactly() {
    let request_target: u64 = kani::any();
    let selected = exact_credential_realization_target(&request_target);
    assert_eq!(*selected, request_target);
    assert!(std::ptr::eq(selected, &request_target));
}

#[cfg(test)]
mod credential_target_projection_tests {
    use super::exact_credential_realization_target;

    #[test]
    fn materialization_target_is_the_original_typed_request_target() {
        let target = awaken_session_contract::McpTarget::parse_http(
            "https://credential-bound.example.test/mcp?tenant=exact",
        )
        .expect("valid target");
        let selected = exact_credential_realization_target(&target);
        assert!(std::ptr::eq(selected, &target));
        assert_eq!(selected, &target);
        assert_eq!(
            selected.http_url(),
            Some("https://credential-bound.example.test/mcp?tenant=exact")
        );
    }
}

#[cfg(test)]
mod session_user_run_input_tests {
    use super::session_user_run_messages;
    use awaken_agent_contract::agent::{
        content::ContentBlock,
        message::{Id as MessageId, Role},
        run::Id as RunId,
    };
    use awaken_session_contract::{SessionUserRunCommand, SessionUserRunSystemInput};

    fn command(system: Option<SessionUserRunSystemInput>) -> SessionUserRunCommand {
        SessionUserRunCommand {
            session_id: "session-input".into(),
            agent_id: "agent".into(),
            operation_id: "user-op".into(),
            run_id: RunId("run-input".into()),
            content: vec![ContentBlock::text("user")],
            accompanying_system: system,
            data_subject_id: None,
            traceparent: None,
        }
    }

    #[test]
    fn reservation_input_freezes_the_accompanying_system_before_user() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 accompanying System absent/present; C2 its
        // operation/content complete/incomplete. Effects: E1 User-only input;
        // E2 stable System then stable User in one vector; E3 invalid input is
        // rejected before dispatch reservation.
        //
        // | Rule | C1 | C2 | Effect |
        // | I1 | absent | - | E1 |
        // | I2 | present | complete | E2 |
        // | I3 | present | empty operation/content | E3 |
        let user_only = session_user_run_messages(&command(None)).expect("I1");
        assert_eq!(user_only.len(), 1, "I1/E1");
        assert_eq!(user_only[0].role, Role::User, "I1/E1");

        let with_system = session_user_run_messages(&command(Some(SessionUserRunSystemInput {
            operation_id: "system-op".into(),
            content: vec![ContentBlock::text("system")],
        })))
        .expect("I2");
        assert_eq!(with_system.len(), 2, "I2/E2");
        assert_eq!(with_system[0].role, Role::System, "I2/E2");
        assert_eq!(with_system[1].role, Role::User, "I2/E2");
        assert_eq!(
            with_system[0].id,
            MessageId::session_system("session-input", "system-op"),
            "I2/E2"
        );
        assert_eq!(
            with_system[1].id,
            MessageId::session_event_input("session-input", "user-op"),
            "I2/E2"
        );

        assert!(
            session_user_run_messages(&command(Some(SessionUserRunSystemInput {
                operation_id: String::new(),
                content: vec![ContentBlock::text("system")],
            })))
            .is_err(),
            "I3/E3"
        );
    }
}

/// Map a neutral terminal state to the Managed idle `stop_reason`. `RequiresAction`
/// carries no event ids here; the projection refills them from the pending tool.
/// The Managed Agents `SessionRuntime` port implemented over the shared host.
/// Holds only an `Arc<SharedHost>` plus runtime-side materialization SPIs, so it
/// configures with any other adapter bound to the same host.
#[derive(Clone)]
pub struct ManagedHost {
    host: Arc<SharedHost>,
    credentials: Option<PinnedCredentialMaterializer>,
    credential_refresh_factory: Option<Arc<dyn CredentialRefreshFactory>>,
    resource_validator: Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
    repository_binding_verifier:
        Option<Arc<dyn RepositoryBindingVerifier<awaken_run_ingress::RunClaim>>>,
    mcp_realizer: Option<Arc<dyn awaken_session_contract::McpAttachmentRealizer>>,
}

/// Composition failure for the one process-local Session coordination port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AgentCoordinationInstallError {
    #[error("Session Agent coordination application is already installed")]
    AlreadyInstalled,
}

/// One compiled projection from the frozen Session manifest. Standard mounts
/// and optional automatic-memory candidates travel together so installation
/// cannot publish one generation with bindings from another.
struct CompiledEffectiveInputs {
    staged: crate::provisioning::StagedResources,
    memory_bindings: std::collections::HashMap<String, Arc<crate::memory::BoundMemory>>,
}

impl ManagedHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self {
            host,
            credentials: None,
            credential_refresh_factory: None,
            resource_validator: None,
            repository_binding_verifier: None,
            mcp_realizer: None,
        }
    }

    /// Connect the fixed Runtime coordination tools to the canonical Session
    /// application after composition has wrapped that application in an `Arc`.
    /// The Host retains only a weak application port, avoiding an ownership
    /// cycle (`SessionApplication -> ManagedHost -> SharedHost`).
    pub fn install_agent_coordination_application(
        &self,
        application: std::sync::Weak<dyn awaken_session_contract::SessionAgentCoordination>,
    ) -> Result<(), AgentCoordinationInstallError> {
        let mut installed = self
            .host
            .agent_coordination
            .write()
            .expect("Agent coordination application lock poisoned");
        if installed.is_some() {
            return Err(AgentCoordinationInstallError::AlreadyInstalled);
        }
        *installed = Some(application);
        Ok(())
    }

    /// Install the fully configured Managed adapter used by durable dispatch.
    ///
    /// Call this once at the process startup after all `with_*` configuration
    /// has been applied. Construction and configuration are deliberately free
    /// of shared-host side effects, so a partially configured adapter can never
    /// become visible to a concurrently claimed Run.
    #[must_use]
    pub fn install_dispatch_session_runtime(self) -> Self {
        *self
            .host
            .dispatch_session_runtime
            .write()
            .expect("dispatch Session Runtime lock poisoned") = Some(DispatchSessionRuntime {
            host: Arc::downgrade(&self.host),
            credentials: self.credentials.clone(),
            credential_refresh_factory: self.credential_refresh_factory.clone(),
            resource_validator: self.resource_validator.clone(),
            repository_binding_verifier: self.repository_binding_verifier.clone(),
            mcp_realizer: self.mcp_realizer.clone(),
        });
        self
    }

    /// Project the committed attempt result into the Managed Session contract.
    /// Output persistence already happened at the shared attempt executor edge,
    /// before direct or durable delivery returns here.
    async fn finish_step(
        &self,
        _thread: &str,
        result: Result<CommittedStepReceipt, HostError>,
    ) -> Result<StepOutcome, RunError> {
        result
            .map_err(to_run_error)
            .and_then(crate::step_projection::settled_step)
    }

    /// Wire the live resource-invariant port used at activation and Memory use.
    /// Configuration was already selected by the Session control plane; this port
    /// only validates trusted Workspace ownership, lifecycle state, and the frozen
    /// config version. It does not make an authorization decision.
    #[must_use]
    pub fn with_resource_validator(
        mut self,
        validator: Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>,
    ) -> Self {
        self.resource_validator = Some(validator);
        self
    }

    /// Install the Repository-specific live binding guard used by a distributed
    /// Worker without granting it Resource Registry database access.
    #[must_use]
    pub fn with_repository_binding_verifier(
        mut self,
        verifier: Arc<dyn RepositoryBindingVerifier<awaken_run_ingress::RunClaim>>,
    ) -> Self {
        self.repository_binding_verifier = Some(verifier);
        self
    }

    /// Realize an already-resolved, secret-free manifest. The pinned Memory/
    /// Repository configuration in `inputs` remains authoritative; the per-item
    /// validation in `stage_resolved_input` checks only current ownership/state
    /// and the frozen version's integrity. No Agent binding or current config is
    /// configured here.
    async fn compile_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<CompiledEffectiveInputs, RunError> {
        let mut all = crate::provisioning::StagedResources::default();
        let mut memory_bindings = std::collections::HashMap::new();
        for input in inputs.inputs() {
            let one = self.stage_resolved_input(workspace, input, claim).await?;
            // Read this exact projection before merging it. Two bindings may
            // legally reference the same store with different access, and a
            // prior mount must never become the authority for the later one.
            let materialization_reference = one.mounts.iter().find_map(|mount| {
                if let awaken_provisioning_contract::MountSource::MemoryStore {
                    materialization_reference,
                    ..
                } = &mount.source
                {
                    materialization_reference.clone()
                } else {
                    None
                }
            });
            if let Some((binding_id, memory)) = self
                .compile_memory_binding(thread, workspace, input, materialization_reference)
                .await?
            {
                memory_bindings.insert(binding_id, memory);
            }
            all.mounts.extend(one.mounts);
            all.prompts.extend(one.prompts);
            all.memory_prompts.extend(one.memory_prompts);
            all.binding_checks.extend(one.binding_checks);
            all.repositories.extend(one.repositories);
        }

        Ok(CompiledEffectiveInputs {
            staged: all,
            memory_bindings,
        })
    }

    /// Compile the one Memory-specific leaf shared by ordinary Session staging
    /// and post-commit recovery from a frozen dispatch. The caller owns Resource
    /// selection and may install the result into a resident Session slot; this
    /// leaf only binds one already-resolved input and never opens an Environment.
    async fn compile_memory_binding(
        &self,
        session_thread: &str,
        workspace: &str,
        input: &awaken_session_contract::ResolvedInput,
        materialization_reference: Option<String>,
    ) -> Result<Option<(String, Arc<crate::memory::BoundMemory>)>, RunError> {
        let awaken_session_contract::ResolvedInputSource::MemoryStore {
            memory_store_id,
            config,
        } = &input.source
        else {
            return Ok(None);
        };
        let writable = input.access == awaken_resource_contract::ResourceAccess::ReadWrite;
        if let Some(reference) = &materialization_reference {
            // Remote Memory claim decision table: active + exact config =>
            // snapshot preflight succeeds; archived/config-changed/stale claim
            // fails before the mounter reuses a prior projection.
            self.host
                .memory_repository()
                .snapshot_heads(reference)
                .await
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        }
        let handle = self.host.platform_memory_handle(
            materialization_reference
                .clone()
                .unwrap_or_else(|| memory_store_id.to_string()),
            writable,
        );
        let resource_validator = if materialization_reference.is_some() {
            None
        } else {
            Some(
                self.resource_validator
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "Memory extraction requires a configured resource binding validator",
                        )
                    })?
                    .clone(),
            )
        };
        Ok(Some((
            input.binding_id.to_string(),
            Arc::new(self.host.memory.bind(
                session_thread,
                workspace,
                handle,
                resource_validator,
                config,
                writable,
            )),
        )))
    }

    async fn install_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        compiled: CompiledEffectiveInputs,
    ) -> Result<(), RunError> {
        // The complete manifest replaces the prior projection. Register an empty
        // value too, so deleting the final input cannot leave a stale mount behind.
        self.host.register_thread_resources(thread, compiled.staged);
        self.host.register_thread_resource_manifest(
            thread,
            awaken_session_contract::SessionResourceManifest::at_revision(
                workspace,
                resource_revision,
                inputs.clone(),
            ),
        );
        // Standard mounts and the optional automatic-memory selection are
        // separate facts. Installing a manifest never picks a "first" store.
        self.host
            .register_thread_memory_bindings(thread, compiled.memory_bindings);
        Ok(())
    }

    async fn stage_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        let compiled = self
            .compile_effective_inputs(thread, workspace, inputs, claim)
            .await?;
        self.install_effective_inputs(thread, workspace, resource_revision, inputs, compiled)
            .await
    }

    /// Install one already-resolved Session resource manifest. This is shared by
    /// managed Session creation and cold durable workers; neither path reads Agent
    /// defaults or selects a newer mutable-resource configuration.
    async fn stage_resource_manifest(
        &self,
        thread: &str,
        workspace: &str,
        resource_revision: u64,
        resources: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        self.host.register_thread_workspace(thread, workspace);
        let desired = awaken_session_contract::SessionResourceManifest::at_revision(
            workspace,
            resource_revision,
            resources.clone(),
        );
        // The authority-side recovery path may first reconcile a retained or
        // pending generation and then prepare the complete Session projection.
        // Both operations carry the same canonical manifest. Avoid compiling and
        // installing it twice; claimed Workers remain excluded because every
        // claim must revalidate live Resource state even when the pin is equal.
        if claim.is_none() && self.host.thread_resource_manifest(thread).as_ref() == Some(&desired)
        {
            return Ok(());
        }
        let versions = self
            .host
            .skills
            .load_pinned(workspace, resources.skills(), claim)
            .await
            .map_err(skill_store_run_error)?;
        self.host
            .session_slots
            .update(thread, |slot| slot.skills = Some(versions));
        self.stage_effective_inputs(thread, workspace, desired.revision, resources, claim)
            .await
    }

    async fn validate_thread_resource_bindings(&self, thread: &str) -> Result<(), RunError> {
        use crate::provisioning::ResourceBindingCheck;

        // This method is entered only through the SessionRuntime application
        // port. Preserve that neutral identity before dispatch so a claiming
        // Worker enters the frozen Session realization path.
        self.host
            .session_slots
            .update(thread, |slot| slot.session_dispatch = true);
        let checks = self
            .host
            .session_slots
            .read(thread, |slot| slot.resources.binding_checks.clone())
            .unwrap_or_default();
        if checks.is_empty() {
            return Ok(());
        }
        let workspace = self.host.thread_workspace(thread);
        for check in checks {
            match check {
                ResourceBindingCheck::MemoryStore {
                    memory_store_id,
                    config_version,
                } => self
                    .resource_validator
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "memory resources require a configured resource binding validator",
                        )
                    })?
                    .verify_memory_binding(&workspace, &memory_store_id, config_version)
                    .map_err(|error| RunError::bad_request(error.to_string()))?,
                ResourceBindingCheck::Repository {
                    repository_id,
                    config_version,
                    claim,
                } => self
                    .repository_binding_verifier
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "repository resources require a configured binding verifier",
                        )
                    })?
                    .verify(&workspace, &repository_id, config_version, claim.as_ref())
                    .await
                    .map(|_| ())
                    .map_err(|error| RunError::bad_request(error.to_string()))?,
            }
        }
        Ok(())
    }

    /// Wire runtime credential injection for the already-frozen Session bindings
    /// and Repository realization.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_credentials(
        self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    ) -> Self {
        self.with_credential_materializer(PinnedCredentialMaterializer::new(credentials, secrets))
    }

    /// Reuse the process startup's canonical exact materializer for MCP and
    /// Repository realization instead of constructing a peer over the same stores.
    #[must_use]
    pub fn with_credential_materializer(
        mut self,
        materializer: PinnedCredentialMaterializer,
    ) -> Self {
        self.credentials = Some(materializer);
        self
    }

    /// Install the credential adapter's exact OAuth refresh port. The Host
    /// retains only this factory and never receives Credential/Secret Store
    /// handles.
    #[must_use]
    pub fn with_credential_refresh_factory(
        mut self,
        factory: Arc<dyn CredentialRefreshFactory>,
    ) -> Self {
        self.credential_refresh_factory = Some(factory);
        self
    }

    /// Replace the local Host MCP realization adapter with one downstream
    /// implementation of the same exact-generation Session port. This is the
    /// sole injection seam used by durable Worker commands; desired state and
    /// credential selection remain outside the implementation.
    #[must_use]
    pub fn with_mcp_attachment_realizer(
        mut self,
        realizer: Arc<dyn awaken_session_contract::McpAttachmentRealizer>,
    ) -> Self {
        self.mcp_realizer = Some(realizer);
        self
    }

    async fn apply_session_inputs_with_context(
        &self,
        thread: &str,
        workspace_id: &str,
        resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        // Reuse the Session slot's canonical realization mutex. Cold active-active
        // requests may concurrently replay the same durable generation; only one
        // may compare, realize, and publish its process-local projection at a time.
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        self.host.register_thread_workspace(thread, workspace_id);
        let desired_manifest = awaken_session_contract::SessionResourceManifest::at_revision(
            workspace_id,
            resource_revision,
            inputs.clone(),
        );
        // Exact local replays are already converged. Claimed replays are handled
        // by `DispatchSessionRuntime::install`, which re-stages them to revalidate
        // live Resource state before entering this replacement path.
        if self.host.thread_resource_manifest(thread).as_ref() == Some(&desired_manifest) {
            return Ok(());
        }
        let old = self.host.thread_resources_snapshot(thread);
        let old_memory: Vec<_> = old
            .mounts
            .iter()
            .filter_map(|mount| match &mount.source {
                awaken_provisioning_contract::MountSource::MemoryStore { store_id, .. } => Some((
                    mount.mount_id.clone(),
                    store_id.clone(),
                    mount.mount_path.clone(),
                    mount.access,
                )),
                _ => None,
            })
            .collect();
        let desired_memory: Vec<_> = inputs
            .inputs()
            .iter()
            .filter_map(|input| match &input.source {
                awaken_session_contract::ResolvedInputSource::MemoryStore {
                    memory_store_id,
                    ..
                } => Some((
                    input.binding_id.to_string(),
                    memory_store_id.to_string(),
                    crate::managed_resource_projection::managed_resource_mount_path(
                        &input.mount_path,
                    ),
                    match input.access {
                        awaken_resource_contract::ResourceAccess::ReadOnly => {
                            awaken_provisioning_contract::MountAccess::ReadOnly
                        }
                        awaken_resource_contract::ResourceAccess::ReadWrite => {
                            awaken_provisioning_contract::MountAccess::ReadWrite
                        }
                    },
                )),
                _ => None,
            })
            .collect();
        let live_environment = self.host.session_environment(thread).await;
        // Another cold-rehydration request can install this exact manifest while
        // the environment lookup above yields. Re-read the canonical manifest at
        // the decision boundary: equal means the concurrent replay converged;
        // unequal remains a forbidden live Memory mutation.
        let installed_manifest = self.host.thread_resource_manifest(thread);
        if live_environment.is_some() && installed_manifest.as_ref() == Some(&desired_manifest) {
            return Ok(());
        }
        // A durable sandbox binding can be adopted before this process has any
        // Resource projection. `None` therefore means cold recovery: install the
        // authority's active generation. Only an already-installed, different
        // manifest is evidence of a forbidden live Memory mutation.
        if live_environment.is_some()
            && installed_manifest.is_some()
            && old_memory != desired_memory
        {
            tracing::warn!(
                session_id = thread,
                installed_manifest = ?installed_manifest,
                old_memory = ?old_memory,
                desired_memory = ?desired_memory,
                "rejecting a live Session Memory projection change"
            );
            return Err(RunError::bad_request(
                "memory_store inputs are create-time only for a live Session",
            ));
        }
        let skill_versions = Some(
            self.host
                .skills
                .load_pinned(workspace_id, inputs.skills(), claim)
                .await
                .map_err(skill_store_run_error)?,
        );
        let compiled = self
            .compile_effective_inputs(thread, workspace_id, inputs, claim)
            .await?;
        let new = &compiled.staged;
        if let Some(environment) = &live_environment {
            environment
                .validate_live_mount_replacement(&old.mounts, &new.mounts)
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        }
        self.host
            .harvest_thread_skills(thread)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        self.host
            .publish_thread_repositories(thread)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        let projection_update = match &live_environment {
            Some(environment) => environment
                .begin_live_projection_update()
                .await
                .map_err(|error| RunError::internal(error.to_string()))?,
            None => None,
        };
        if let Some(environment) = &live_environment {
            // Realize the desired live projection before committing its logical
            // manifest. Every operation is idempotent, so a failed attempt leaves
            // the prior manifest authoritative and the persisted pending generation
            // can safely retry without mistaking an unrealized mount for success.
            environment
                .remove_projection_path(crate::skills::DELIVERED_SKILLS_SUBDIR)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?;
            for mount in &old.mounts {
                if !new
                    .mounts
                    .iter()
                    .any(|candidate| candidate.mount_path == mount.mount_path)
                {
                    environment
                        .remove_projection_path(&mount.mount_path)
                        .await
                        .map_err(|error| RunError::internal(error.to_string()))?;
                }
            }
            for mount in &new.mounts {
                if !old.mounts.iter().any(|candidate| candidate == mount) {
                    environment
                        .attach_mount(mount.clone())
                        .await
                        .map_err(|error| RunError::internal(error.to_string()))?;
                }
            }
            for repository in &old.repositories {
                if !new
                    .repositories
                    .iter()
                    .any(|candidate| candidate.plan == repository.plan)
                {
                    environment
                        .remove_projection_path(&repository.plan.mount_path)
                        .await
                        .map_err(|error| RunError::internal(error.to_string()))?;
                }
            }
            for repository in &new.repositories {
                if !old
                    .repositories
                    .iter()
                    .any(|candidate| candidate.plan == repository.plan)
                {
                    awaken_provisioning_contract::RepositoryRealizer::realize_repository(
                        environment.as_ref(),
                        &repository.plan,
                        repository.credential.as_ref(),
                    )
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?;
                }
            }
        }
        self.install_effective_inputs(thread, workspace_id, resource_revision, inputs, compiled)
            .await?;
        if let Some(update) = projection_update {
            update.commit();
        }
        self.host
            .session_slots
            .update(thread, |slot| slot.skills = skill_versions);
        self.host.evict_session_for_rebuild(thread).await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionRuntime for ManagedHost {
    fn install_session_baseline(
        &self,
        thread: &str,
        baseline: &awaken_session_contract::SessionBaseline,
    ) -> Result<(), RunError> {
        self.host
            .install_frozen_session_baseline(thread, baseline)
            .map_err(to_run_error)
    }

    fn install_session_request_context(
        &self,
        thread: &str,
        messages: Vec<Message>,
    ) -> Result<(), RunError> {
        self.host.install_session_request_context(thread, messages);
        Ok(())
    }

    fn install_environment_binding_sink(
        &self,
        sink: Arc<dyn awaken_session_contract::SessionEnvironmentBindingSink>,
    ) {
        *self
            .host
            .environment_binding_sink
            .write()
            .expect("environment binding sink lock poisoned") = Some(sink);
    }

    fn install_session_realization_lease(
        &self,
        session_id: &str,
        lease: awaken_session_contract::SessionRealizationLease,
    ) {
        self.host
            .install_session_realization_lease(session_id, lease);
    }

    fn install_expected_environment_binding(
        &self,
        session_id: &str,
        binding: Option<String>,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.host
            .install_expected_environment_binding(session_id, binding)
            .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))
    }

    async fn reserve_session_user_run(
        &self,
        command: awaken_session_contract::SessionUserRunCommand,
    ) -> Result<awaken_session_contract::SessionUserRunReservation, RunError> {
        use awaken_run_ingress::{Clock as _, DispatchQueue as _};

        let input = session_user_run_messages(&command)?;
        self.validate_thread_resource_bindings(&command.session_id)
            .await?;
        let ctx = self
            .host
            .ctx_for(&command.session_id, Some(&command.agent_id))
            .await
            .map_err(to_run_error)?;
        let input = match &ctx.skill_registry {
            Some(registry) => awaken_ext_skills::expand_slash_commands(
                registry.as_ref(),
                &command.session_id,
                input,
            ),
            None => input,
        };
        let (_, mut activation) =
            ctx.runtime
                .prepare(&ctx.config, command.session_id.clone(), input);
        activation.run_id = command.run_id;
        activation.model_ref_override = self
            .host
            .inference_routing
            .override_for(&command.session_id);
        activation.data_subject_id = command
            .data_subject_id
            .map(awaken_runtime_contract::DataSubjectId);
        let request = self
            .host
            .resolved_dispatch_with_traceparent(activation, command.traceparent)
            .map_err(to_run_error)?;
        let deadline = awaken_run_ingress::SystemClock
            .now_ms()
            .saturating_add(30_000);
        match self
            .host
            .dispatch_store()
            .map_err(to_run_error)?
            .reserve_session_run(request, deadline)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?
        {
            awaken_run_ingress::SessionRunReservationOutcome::Reserved => {
                Ok(awaken_session_contract::SessionUserRunReservation::Reserved)
            }
            awaken_run_ingress::SessionRunReservationOutcome::AlreadyReserved => {
                Ok(awaken_session_contract::SessionUserRunReservation::AlreadyReserved)
            }
            awaken_run_ingress::SessionRunReservationOutcome::RecoveryClaimed => {
                Ok(awaken_session_contract::SessionUserRunReservation::RecoveryClaimed)
            }
            awaken_run_ingress::SessionRunReservationOutcome::AlreadyActivated {
                session_activity_epoch,
            } => Ok(
                awaken_session_contract::SessionUserRunReservation::AlreadyActivated {
                    session_activity_epoch,
                },
            ),
            awaken_run_ingress::SessionRunReservationOutcome::Completed => {
                Ok(awaken_session_contract::SessionUserRunReservation::Completed)
            }
            awaken_run_ingress::SessionRunReservationOutcome::Conflict => Err(
                RunError::bad_request("Session User Run id was reused with different input"),
            ),
        }
    }

    async fn activate_session_user_run(
        &self,
        delivery: awaken_session_contract::SessionUserRunDelivery,
    ) -> Result<awaken_session_contract::SessionUserRunActivation, RunError> {
        self.host
            .activate_session_user_run_reservation(delivery)
            .await
            .map_err(to_run_error)
    }

    async fn activate_and_observe_session_user_run(
        &self,
        admission: awaken_session_contract::SessionUserRunAdmission,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_agent_contract::agent::run::RunState, RunError> {
        self.host
            .activate_and_observe_session_user_run(admission, sink)
            .await
            .map_err(to_run_error)
    }

    async fn session_user_run_state(
        &self,
        session_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
        let ctx = self
            .host
            .ctx_for(session_id, None)
            .await
            .map_err(to_run_error)?;
        ctx.commit
            .authoritative_run(run_id)
            .await
            .map(|record| record.map(|record| record.state))
            .map_err(|error| RunError::unavailable(error.to_string()))
    }

    async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.host.delegated_runs(thread).await.map_err(to_run_error)
    }

    async fn admit_coordinated_run(
        &self,
        command: awaken_session_contract::CoordinatedRunCommand,
    ) -> Result<awaken_session_contract::SessionAgentMessageReceipt, RunError> {
        self.host
            .admit_coordinated_run(command)
            .await
            .map_err(to_run_error)
    }

    async fn coordinated_threads(
        &self,
        session_id: &str,
    ) -> Result<Vec<awaken_session_contract::CoordinatedThreadLink>, RunError> {
        self.host
            .coordinated_threads(session_id)
            .await
            .map_err(to_run_error)
    }

    async fn subscribe_session_thread_live(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<Box<dyn awaken_session_contract::SessionThreadLiveSubscription>>, RunError>
    {
        if thread_id == session_id {
            return Ok(Some(self.host.hub.live_subscription(thread_id)));
        }
        let links = self
            .host
            .coordinated_threads(session_id)
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

    async fn session_thread_disposition(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, RunError> {
        self.host
            .session_thread_disposition(
                session_id,
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            )
            .await
            .map_err(to_run_error)
    }

    async fn archive_session_thread(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<(), RunError> {
        self.host
            .archive_session_thread(
                session_id,
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            )
            .await
            .map_err(to_run_error)
    }

    async fn continue_session_agent_report(
        &self,
        command: awaken_session_contract::SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        self.host
            .continue_session_agent_report(command)
            .await
            .map_err(to_run_error)
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &str,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), RunError> {
        self.host
            .interrupt_session_thread(session_id, child_thread_id)
            .await
            .map_err(to_run_error)
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_contract::SessionThreadToolReplyFence, RunError> {
        self.host
            .session_thread_tool_reply_fence(command)
            .await
            .map_err(to_run_error)
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        self.host
            .reply_session_thread_tool(delivery)
            .await
            .map_err(to_run_error)
    }

    async fn resume_budget_reached(
        &self,
        delivery: awaken_session_contract::SessionBudgetResumeDelivery,
    ) -> Result<awaken_session_contract::SessionBudgetResumeDisposition, RunError> {
        self.host
            .resume_budget_reached(delivery)
            .await
            .map_err(to_run_error)
    }

    async fn session_budget_resume_tickets(
        &self,
        session_id: &str,
    ) -> Result<Vec<awaken_session_contract::SessionBudgetResumeTicket>, RunError> {
        self.host
            .session_budget_resume_tickets(session_id)
            .await
            .map_err(to_run_error)
    }

    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_contract::DelegatedRunSnapshot, RunError> {
        self.host
            .quiesce_terminal_delegations(thread)
            .await
            .map_err(to_run_error)
    }

    async fn owns_thread(&self, thread: &str) -> Result<bool, RunError> {
        self.host
            .has_durable_thread(thread)
            .await
            .map_err(to_run_error)
    }

    async fn execute_terminal_cleanup(
        &self,
        command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        // Archive/delete and the recovery scanner may observe the same durable
        // cleanup intent concurrently. Serialize the complete external-effect
        // sequence on the Session lifecycle owner: repository push is CAS-based
        // and cannot safely race an identical retry before the first receipt is
        // committed. Once the winner removes the slot, the waiter sees an empty
        // projection and completes as the intended idempotent no-op.
        let lifecycle = self
            .host
            .session_slots
            .update(&command.thread_id, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        // Terminal release owns every reverse operation: publish Agent-authored Repo
        // commits (when the Agent did not own publication through MCP), persist
        // run-authored Skills, then dispose. A GET /files poll is never a write edge.
        self.host
            .publish_thread_repositories(&command.thread_id)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        self.host
            .harvest_thread_skills(&command.thread_id)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        // Failure is terminal-release blocking: keep the Sandbox available for
        // the durable cleanup retry instead of disposing unharvested outputs.
        let artifact_receipts = self
            .host
            .harvest_thread_artifacts(&command.thread_id)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        // Memory is owned by its MemoryMount guard: FUSE writes through live and
        // copy realization performs one CAS harvest during teardown.
        self.host
            .end_session(&command.thread_id)
            .await
            .map_err(to_run_error)?;
        let completion =
            awaken_session_contract::SessionCleanupCompletion::new(&command, artifact_receipts);
        completion
            .verify(&command)
            .map_err(|error| RunError::internal(error.to_string()))?;
        Ok(completion)
    }

    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        let result = self
            .host
            .run(Some(agent), thread, vec![user_message(content)])
            .await;
        self.finish_step(thread, result).await
    }

    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        let result = self
            .host
            .run_attributed(
                Some(agent),
                thread,
                vec![user_message(content)],
                data_subject_id.map(awaken_runtime_contract::DataSubjectId),
            )
            .await;
        self.finish_step(thread, result).await
    }

    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        // Same committed Run as `run`; `sink` mirrors in-flight `stream::Kind` so
        // the Managed adapter can project live `agent.message` previews.
        let result = self
            .host
            .run_streaming(Some(agent), thread, vec![user_message(content)], sink)
            .await;
        self.finish_step(thread, result).await
    }

    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
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
        self.finish_step(thread, result).await
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        let result = self
            .host
            .resume(thread, tool_use_id, HostResume::Permission(decision))
            .await;
        self.finish_step(thread, result).await
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        let result = self
            .host
            .resume(
                thread,
                tool_use_id,
                HostResume::ClientResult { content, is_error },
            )
            .await;
        self.finish_step(thread, result).await
    }

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

    async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        managed_model_capability::supports_mid_conversation_system(
            &self.host.model_for_thread(thread),
        )
    }

    async fn pending_tool(&self, thread: &str) -> Result<Option<Pending>, RunError> {
        self.host.pending_tool(thread).await.map_err(to_run_error)
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.host.interrupt(thread).await.map_err(to_run_error)
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

    async fn continue_outcome(&self, thread: &str) -> Result<Option<OutcomeDrive>, RunError> {
        managed_outcome::resume(&self.host, thread).await
    }

    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<awaken_session_contract::CommittedOutcomeProjection>, RunError> {
        managed_outcome::committed_projection(&self.host, thread, outcome_id).await
    }

    /// Replace only the already-selected model projection for one Session and
    /// force its next context construction to consume that exact selection.
    async fn rebind_model(&self, thread: &str, model: &str) -> Result<(), RunError> {
        // R5: re-stage the thread's model and evict its cached context so the next
        // Run rebuilds with the newly resolved executor (native switch is O(1); an
        // ACP thread's cached context relaunches its CLI on rebuild).
        self.host.register_thread_model(thread, model);
        self.host.evict_session_for_rebuild(thread).await;
        Ok(())
    }

    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<awaken_session_contract::ResolvedSkillBinding>, RunError> {
        if skills.is_empty() {
            return Ok(Vec::new());
        }
        self.host
            .skills
            .resolve(workspace_id, skills)
            .await
            .map_err(skill_store_run_error)
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        workspace_id: &str,
        resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        self.apply_session_inputs_with_context(
            thread,
            workspace_id,
            resource_revision,
            inputs,
            None,
        )
        .await
    }

    async fn prepare_session(
        &self,
        thread: &str,
        init: awaken_session_contract::SessionInit,
    ) -> Result<(), RunError> {
        // Session preparation and durable Resource reconciliation publish one
        // process-local projection. Serialize both through the existing slot
        // lifecycle so a peer request cannot observe Environment installed while
        // the exact frozen Resource manifest is still absent.
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        // A process may have opened this durable thread before its Control-frozen
        // projection arrived (for example a peer/recovery read racing Session
        // rehydration). That context was necessarily built from host defaults.
        // Projection installation is the authority transition: discard only the
        // rebuildable context while retaining any independently-owned Environment.
        // A live run cannot be rebound underneath its already-created activation.
        let active_projection = self
            .host
            .session_slots
            .read(thread, |slot| {
                (
                    slot.runtime.as_ref().and_then(|context| {
                        context
                            .active_run
                            .lock()
                            .expect("active run mutex poisoned")
                            .clone()
                    }),
                    slot.baseline.is_some() || slot.session_dispatch,
                )
            })
            .unwrap_or((None, false));
        if active_projection.0.is_some() && active_projection.1 {
            // The Session application has already installed this durable
            // dispatch projection (or a claimed Worker installed the complete
            // immutable baseline), and the live context was necessarily built
            // after that authority transition. A successor event may be admitted
            // while the preceding Run is finishing; its execution mutex provides
            // ordering, so keep the exact resident projection instead of rebinding.
            return Ok(());
        }
        if active_projection.0.is_some() {
            return Err(RunError::internal(
                "cannot install a frozen Session projection while its Runtime is active",
            ));
        }
        self.host.session_slots.update(thread, |slot| {
            slot.runtime = None;
            slot.session_dispatch = true;
        });
        // This is the one projection lowering path shared with claimed Worker
        // replay. In particular, workspace/Agent/backend cannot drift between
        // Coordinator dispatch construction and Worker execution.
        self.host
            .project_session_init(thread, &init)
            .map_err(to_run_error)?;
        if init.environment.sandbox_provisioning
            == awaken_session_contract::SandboxProvisioning::OnToolUse
        {
            let executor: Arc<dyn awaken_runtime_contract::tool::ToolExecutor> =
                Arc::new(crate::lazy_sandbox::DeferredSandboxExecutor::new(
                    Arc::downgrade(&self.host),
                    thread,
                ));
            self.host.session_slots.update(thread, |slot| {
                slot.deferred_executor = Some(executor);
            });
        }
        // Stage only the already-resolved manifest. Runtime never reads the Agent
        // binding repository or configures defaults again.
        self.stage_resource_manifest(
            thread,
            &init.workspace_id,
            init.resource_revision,
            &init.resources,
            None,
        )
        .await?;
        Ok(())
    }

    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_contract::SessionToolConfiguration,
    ) -> Result<(), RunError> {
        self.host
            .session_slots
            .update(thread, |slot| slot.tools = Some(tools));
        self.host.evict_session_for_rebuild(thread).await;
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
        let (_, _, publication) = self
            .host
            .resolve_session_publication(thread, Some(agent), None)
            .map_err(to_run_error)?;
        let provisioning = publication
            .as_ref()
            .map(|snapshot| snapshot.resolved_spec.model_binding.provisioning())
            .unwrap_or(&awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor);
        let (adopted, rebuild) = self
            .host
            .adopt_bound_session_environment(thread, Some(binding), provisioning, false)
            .await
            .map_err(to_run_error)?;
        debug_assert!(!rebuild);
        self.host
            .ctx_for_with_sandbox(thread, Some(agent), adopted)
            .await
            .map_err(to_run_error)?;
        Ok(())
    }

    async fn quiesce_session_environment(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
    ) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
        self.quiesce_environment_continuation(thread, operation, generation)
            .await
    }

    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_contract::CheckpointReceipt, RunError> {
        self.checkpoint_environment_continuation(thread, request)
            .await
    }

    async fn dispose_checkpoint_source(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_contract::SourceDisposedReceipt, RunError> {
        self.dispose_environment_continuation_source(thread, operation, generation, source_binding)
            .await
    }

    async fn restore_checkpointed_session_environment(
        &self,
        agent: &str,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
        checkpoint: &awaken_session_contract::SandboxCheckpointRef,
    ) -> Result<awaken_session_contract::RestoreReceipt, RunError> {
        self.restore_environment_continuation(agent, thread, operation, generation, checkpoint)
            .await
    }

    async fn delete_session_checkpoint(
        &self,
        _thread: &str,
        checkpoint: &awaken_session_contract::SandboxCheckpointRef,
    ) -> Result<(), RunError> {
        self.delete_environment_continuation_checkpoint(checkpoint)
            .await
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

    async fn committed_run_lifecycle(
        &self,
        thread: &str,
        cursor: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_agent_contract::RunLifecyclePage, RunError> {
        let feed = self
            .host
            .run_lifecycle_feed(thread)
            .await
            .map_err(to_run_error)?;
        awaken_agent_contract::RunLifecycleFeed::events_after(feed.as_ref(), cursor, limit)
            .await
            .map_err(|error| RunError::internal(error.to_string()))
    }

    async fn session_usage(
        &self,
        thread: &str,
    ) -> Result<awaken_session_contract::SessionUsage, RunError> {
        // Map the runtime's per-model tally onto the managed wire's session-level total
        // (the host is the context boundary; the managed crate never sees TokenUsage).
        let attributed = self.host.thread_usage(thread).await;
        let total = attributed.total();
        Ok(awaken_session_contract::SessionUsage {
            input_tokens: total.prompt_tokens,
            output_tokens: total.completion_tokens,
            cache_read_tokens: total.cache_read_tokens,
            cache_creation_tokens: total.cache_creation_tokens,
            by_model: attributed
                .by_model
                .into_iter()
                .map(|(model, usage)| {
                    (
                        model,
                        awaken_session_contract::SessionModelUsage {
                            input_tokens: usage.prompt_tokens,
                            output_tokens: usage.completion_tokens,
                            cache_read_tokens: usage.cache_read_tokens,
                            cache_creation_tokens: usage.cache_creation_tokens,
                        },
                    )
                })
                .collect(),
            active_seconds: 0,
            web_fetch_requests: 0,
            web_search_requests: 0,
        })
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.host
            .session_thread_recovery_snapshot(
                session_id,
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            )
            .await
            .map_err(to_run_error)
    }

    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        let snapshot = self
            .host
            .session_thread_run_recovery_snapshot(
                session_id,
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                run_id,
            )
            .await
            .map_err(to_run_error)?;
        Ok(snapshot
            .runs
            .iter()
            .any(|run| &run.id == run_id)
            .then_some(snapshot))
    }

    async fn session_thread_usage(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<awaken_session_contract::SessionUsage, RunError> {
        self.host
            .session_thread_usage(
                session_id,
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            )
            .await
            .map_err(to_run_error)
    }

    fn model(&self) -> String {
        self.host.model()
    }

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
