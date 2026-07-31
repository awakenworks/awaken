//! `awaken-runtime-host` — the managed-agents SERVICE layer.
//!
//! It owns the protocol-neutral [`SharedHost`] (the thread-keyed session
//! substrate) and the two port adapters mounted over it: [`ManagedHost`] (the
//! Managed Agents `SessionRuntime`) and [`ProtocolHost`] (the neutral
//! `ProtocolRuntime` behind the AI SDK / AG-UI / A2A wire adapters). Both hold
//! the same `Arc<SharedHost>`, so a run started through one protocol can be
//! resumed or observed through another on the *same thread*.
//!
//! The composition root (`awaken-server`) assembles these into routers;
//! this crate carries no wire assembly of its own beyond the per-plane routers
//! it exposes (config / files / memory-stores / durable-ops).

mod acp_backend;
mod acp_provision;
mod acp_serve;
mod acp_tool_export;
mod agent_catalog;
mod agent_runner;
mod application;
mod background;
mod commit_backend;
mod commit_ingest;
mod compact;
mod config;
mod container_environment;
mod delegate;
mod deployment_config;
mod dispatch_backend;
mod dispatch_transport;
mod durable_ops;
mod file_content_transport;
mod hand_placement;
mod host;
mod hub;
mod inference_routing;
mod judge;
mod lazy_sandbox;
mod live_inbox;
mod managed_model_capability;
mod managed_resource_projection;
mod mcp;
mod mcp_relay;
mod memory;
mod memory_stores;
mod memory_transport;
mod no_model;
mod outcome_controller;
mod provisioning;
mod redact;
mod repository_transport;
mod resource_reclamation;
pub use resource_reclamation::HostResourceReclamation;
mod run_exec;
mod sandbox_source;
mod session_environment;
mod session_slot;
pub use session_environment::HandExecutorFactory;
mod skill_bundle_transport;
mod skill_catalog;
mod skills;
mod store;
#[cfg(test)]
mod test_mcp;
mod tool_output_spill;
mod web_search;
mod worker_http;

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_protocol_transport::{
    DriverError, Pending as PortPending, ProtocolRuntime, Resume as PortResume,
    StepFailure as PortStepFailure, StepOutcome as PortStepOutcome, Terminal,
};
use awaken_runtime_contract::live_inbox::{EditError, LiveInboxMessageId, MessageOrigin, Offer};
use awaken_session_contract::{
    AgentCapabilities, BuiltinTool, CustomTool, DelegatedRun, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, OutcomeIteration, OutcomeReport, Pending, RunError, SessionRuntime,
    StepOutcome, ToolPermissionDecision,
};

use crate::host::{HostError, HostErrorKind, PendingTool, RunResult};
mod postgres_migration_lock;
mod worker_control_client;

// The neutral session substrate and its resume vocabulary.
pub use crate::acp_tool_export::{AcpToolExport, AcpToolExporter};
pub use crate::application::{
    ApplicationSessionControlClient, ApplicationSessionControlReceipt, ApplicationSessionError,
    ApplicationSessionProvisioner, WorkerControlApplicationSessionClient,
};
pub use crate::commit_backend::init_shared_postgres_commit;
pub use crate::dispatch_backend::init_shared_postgres_dispatch_with_config;
pub use crate::file_content_transport::{
    FileContentSource, FileContentSourceError, HttpFileContentSource, StoreFileContentSource,
    WorkerFileContentService, worker_file_content_router,
};
pub use crate::host::{
    AttemptExecutorDecorator, HostResume, RemoteAttemptInstallation, SharedHost,
    remote_worker_placement, self_hosted_inference_holder,
};
pub use crate::memory_transport::{
    HttpMemoryRepository, HttpMemorySnapshotSource, HttpMemoryWritebackClient, WorkerMemoryService,
    memory_materialization_reference, worker_memory_router,
};
pub use crate::no_model::{NoModelConfiguredExecutor, UNCONFIGURED_MODEL_REF};
pub use crate::postgres_migration_lock::PostgresMigrationLock;
pub use crate::repository_transport::{
    CatalogRepositoryBindingVerifier, HttpRepositoryBindingVerifier, RepositoryBindingVerifier,
    RepositoryBindingVerifierError, WorkerRepositoryBindingService,
    worker_repository_binding_router,
};
pub use crate::skill_bundle_transport::{
    HttpSkillBundleSource, SkillBundleSource, SkillBundleSourceError, StoreSkillBundleSource,
    WorkerSkillBundleService, worker_skill_bundle_router,
};
pub use crate::worker_control_client::WorkerControlClient;
use awaken_credential_materializer::PinnedCredentialMaterializer;
// ACP launch projection consumes the Session environment selected by the host.
pub use crate::hub::{ThreadEvent, ThreadEventHub};
pub use crate::redact::PiiRedactor;
pub use crate::sandbox_source::{AcpLaunchRegistry, LaunchSource, resolve_sandbox_tier};
pub use crate::skills::SkillForkPlacement;
// The config data plane (ADR-0036/slice A): the service + its router + the
// advertised-tools helper the composition root builds a config host from.
pub use crate::acp_provision::PublishedAcpLaunchResolver;
pub use crate::acp_serve::{AcpServeHost, AcpStop, AcpTurn};
pub use crate::config::{
    advertised_tools, authorable_config_sections, authorable_config_sections_with_web_search,
    authorable_tools, block_text, platform_plugin_capabilities,
    platform_plugin_capabilities_with_web_search,
};
// The per-plane resource routers the composition root merges over one host.
pub use crate::commit_ingest::{
    ClaimedCommitService, RemoteClaimedRunCommit, claimed_commit_router,
};
pub use crate::deployment_config::{
    AcpWorkerProfile, ContentCaptureSettings, ContentRedaction, DeploymentConfig, DispatchBackend,
    SandboxSettings, SandboxTier, StoreKind, Wake, default_postgres_max_connections,
};
pub use crate::dispatch_transport::{
    WorkerDispatchService, dispatch_transport_router_with_service,
    registered_worker_transport_router, registered_worker_transport_router_with_services,
    worker_dispatch_store_with_upstream,
};
pub use crate::durable_ops::durable_ops_router;
// The model-route seam (R1/R2/R5): a composition root supplies its own
// `InferenceExecutorMaterializer` to map a session's model ref to a labeled executor.
pub use crate::inference_routing::InferenceExecutorMaterializer;
// The managed-vault OAuth seams (ADR-0043): the transport-level refresher, its
// prepared configuration, and the live MCP credential probe.
pub use crate::mcp::{ExtMcpProbe, VaultRefresher};
// ── Managed Agents adapter over the shared host ─────────────────────────────

/// Translate the runtime contract's edit refusal into the wire-facing error.
/// `Closed` collapses into `Inactive`: from the client's view "the attempt is
/// gone" and "no attempt is running" are the same condition.
fn to_live_inbox_error(err: EditError) -> LiveInboxError {
    match err {
        EditError::Closed => LiveInboxError::Inactive,
        EditError::UnknownMessage => LiveInboxError::UnknownMessage,
        EditError::StaleOrder => LiveInboxError::StaleOrder,
    }
}

/// Mint a fresh user message from plain text (Managed `user.message` content is
/// concatenated to text before it enters the host).
fn user_message(content: Vec<ContentBlock>) -> Message {
    Message::new(
        MessageId(awaken_runtime::fresh_process_id("usr")),
        Role::User,
        content,
    )
}

fn to_run_error(err: HostError) -> RunError {
    let message = err.message;
    if message.contains("401") || message.to_ascii_lowercase().contains("auth") {
        return RunError::classified("mcp_authentication_failed", message);
    }
    if message.to_ascii_lowercase().contains("mcp server") {
        return RunError::classified("mcp_connection_failed", message);
    }
    match err.kind {
        HostErrorKind::BadRequest => RunError::bad_request(message),
        HostErrorKind::Internal => RunError::internal(message),
    }
}

/// Map a neutral terminal state to the Managed idle `stop_reason`. `RequiresAction`
/// carries no event ids here; the projection refills them from the pending tool.
fn to_pending(pending: Option<PendingTool>) -> Option<Pending> {
    pending.map(|p| Pending {
        tool_use_id: p.tool_use_id,
        name: p.name,
        input: p.input,
        client_executed: p.client_executed,
    })
}

fn to_step_outcome(result: RunResult) -> Result<StepOutcome, RunError> {
    let delegated_runs = result.delegated_runs;
    match result.state {
        RunState::Awaiting => Ok(StepOutcome::awaiting(
            result.new_messages,
            to_pending(result.pending),
            result.compacted,
            result.rescheduled,
        )
        .with_delegated_runs(delegated_runs)),
        RunState::Ended(cause) => Ok(StepOutcome::ended(
            result.new_messages,
            cause,
            result.compacted,
            result.rescheduled,
        )
        .with_delegated_runs(delegated_runs)),
        RunState::Running => Err(RunError::internal(
            "runtime returned an unsettled Running state at the session boundary",
        )),
    }
}

/// The Managed Agents `SessionRuntime` port implemented over the shared host.
/// Holds only an `Arc<SharedHost>` plus runtime-side materialization SPIs, so it
/// composes with any other adapter bound to the same host.
#[derive(Clone)]
pub struct ManagedHost {
    host: Arc<SharedHost>,
    credentials: Option<PinnedCredentialMaterializer>,
    resource_validator: Option<Arc<dyn awaken_resource_contract::ResourceBindingValidator>>,
    repository_binding_verifier: Option<Arc<dyn RepositoryBindingVerifier>>,
    mcp_realizer: Option<Arc<dyn awaken_session_contract::McpAttachmentRealizer>>,
}

/// Weak, cloneable Worker-side adapter over the same configured Managed
/// `SessionRuntime`. Durable dispatch carries only secret-free projections; this
/// object reuses the installed Resource validator and credential SPIs for
/// Resource and MCP realization without constructing a parallel vault path.
#[derive(Clone)]
pub(crate) struct DispatchSessionRuntime {
    host: std::sync::Weak<SharedHost>,
    credentials: Option<PinnedCredentialMaterializer>,
    resource_validator: Option<Arc<dyn awaken_resource_contract::ResourceBindingValidator>>,
    repository_binding_verifier: Option<Arc<dyn RepositoryBindingVerifier>>,
    mcp_realizer: Option<Arc<dyn awaken_session_contract::McpAttachmentRealizer>>,
}

impl DispatchSessionRuntime {
    fn managed(&self) -> Result<ManagedHost, RunError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RunError::internal("dispatch Session Runtime host was dropped"))?;
        Ok(ManagedHost {
            host,
            credentials: self.credentials.clone(),
            resource_validator: self.resource_validator.clone(),
            repository_binding_verifier: self.repository_binding_verifier.clone(),
            mcp_realizer: None,
        })
    }

    async fn install(
        &self,
        thread: &str,
        manifest: &awaken_session_contract::SessionResourceManifest,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        let managed = self.managed()?;
        let previous = managed.host.thread_resource_manifest(thread);
        if previous
            .as_ref()
            .is_some_and(|previous| previous != manifest)
        {
            return Err(RunError::bad_request(
                "a claimed Worker cannot replace the frozen Session Resource manifest",
            ));
        }
        // Re-stage even when the manifest is unchanged: immutable File bytes,
        // config-version integrity, and credential revocation are live-deny checks
        // at every claimed operation. Worker projection never mutates the
        // authority-side intrinsic Resource reference graph.
        managed
            .stage_resource_manifest(
                thread,
                &manifest.workspace_id,
                &manifest.resources,
                claim,
                false,
            )
            .await?;
        Ok(())
    }

    async fn stage_mcp(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.stage_mcp_attachment(request).await,
            None => {
                awaken_session_contract::McpAttachmentRealizer::stage_mcp_attachment(
                    &self.managed()?,
                    request,
                )
                .await
            }
        }
    }

    async fn publish_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.publish_mcp_generation(generation).await,
            None => {
                awaken_session_contract::McpAttachmentRealizer::publish_mcp_generation(
                    &self.managed()?,
                    generation,
                )
                .await
            }
        }
    }

    async fn drain_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.drain_mcp_generation(generation).await,
            None => {
                awaken_session_contract::McpAttachmentRealizer::drain_mcp_generation(
                    &self.managed()?,
                    generation,
                )
                .await
            }
        }
    }
}

impl SharedHost {
    pub(crate) async fn install_dispatched_resources(
        &self,
        thread: &str,
        manifest: &awaken_session_contract::SessionResourceManifest,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        let preparer = self
            .dispatch_session_runtime
            .read()
            .expect("dispatch Session Runtime lock poisoned")
            .clone()
            .ok_or_else(|| {
                RunError::internal("durable resource dispatch has no Session Runtime")
            })?;
        preparer.install(thread, manifest, claim).await
    }

    fn dispatch_session_runtime(&self) -> Result<DispatchSessionRuntime, RunError> {
        self.dispatch_session_runtime
            .read()
            .expect("dispatch Session Runtime lock poisoned")
            .clone()
            .ok_or_else(|| RunError::internal("dispatch Session Runtime is not configured"))
    }

    pub(crate) async fn stage_dispatched_mcp(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        self.dispatch_session_runtime()?.stage_mcp(request).await
    }

    pub(crate) async fn publish_dispatched_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?
            .publish_mcp(generation)
            .await
    }

    pub(crate) async fn drain_dispatched_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?.drain_mcp(generation).await
    }
}

impl ManagedHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        let managed = Self {
            host,
            credentials: None,
            resource_validator: None,
            repository_binding_verifier: None,
            mcp_realizer: None,
        };
        managed.refresh_dispatch_session_runtime();
        managed
    }

    fn refresh_dispatch_session_runtime(&self) {
        *self
            .host
            .dispatch_session_runtime
            .write()
            .expect("dispatch Session Runtime lock poisoned") = Some(DispatchSessionRuntime {
            host: Arc::downgrade(&self.host),
            credentials: self.credentials.clone(),
            resource_validator: self.resource_validator.clone(),
            repository_binding_verifier: self.repository_binding_verifier.clone(),
            mcp_realizer: self.mcp_realizer.clone(),
        });
    }

    async fn harvest_artifacts(&self, thread: &str) -> Result<(), RunError> {
        self.host
            .harvest_thread_artifacts(thread)
            .await
            .map(|_| ())
            .map_err(|error| RunError::internal(error.to_string()))
    }

    /// One post-step edge for every execution variant. Outputs written before a
    /// failed model/tool step are harvested too; the original execution error
    /// remains the caller-visible failure and terminal release can retry harvest.
    async fn finish_step(
        &self,
        thread: &str,
        result: Result<RunResult, HostError>,
    ) -> Result<StepOutcome, RunError> {
        match result {
            Ok(result) => {
                self.harvest_artifacts(thread).await?;
                to_step_outcome(result)
            }
            Err(error) => {
                let _ = self.harvest_artifacts(thread).await;
                Err(to_run_error(error))
            }
        }
    }

    /// Wire the live resource-invariant port used at activation and Memory use.
    /// Configuration was already selected by the Session control plane; this port
    /// only validates trusted Workspace ownership, lifecycle state, and the frozen
    /// config version. It does not make an authorization decision.
    #[must_use]
    pub fn with_resource_validator(
        mut self,
        validator: Arc<dyn awaken_resource_contract::ResourceBindingValidator>,
    ) -> Self {
        self.repository_binding_verifier = Some(Arc::new(CatalogRepositoryBindingVerifier::new(
            validator.clone(),
        )));
        self.resource_validator = Some(validator);
        self.refresh_dispatch_session_runtime();
        self
    }

    /// Install the Repository-specific live binding guard used by a distributed
    /// Worker without granting it Resource Catalog database access.
    #[must_use]
    pub fn with_repository_binding_verifier(
        mut self,
        verifier: Arc<dyn RepositoryBindingVerifier>,
    ) -> Self {
        self.repository_binding_verifier = Some(verifier);
        self.refresh_dispatch_session_runtime();
        self
    }

    /// Realize an already-resolved, secret-free manifest. The pinned Memory/
    /// Repository configuration in `inputs` remains authoritative; the per-item
    /// validation in `stage_resolved_input` checks only current ownership/state
    /// and the frozen version's integrity. No Agent binding or current config is
    /// composed here.
    async fn compile_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<
        (
            crate::provisioning::StagedResources,
            Option<Arc<crate::memory::BoundMemory>>,
        ),
        RunError,
    > {
        let mut all = crate::provisioning::StagedResources::default();
        let mut bound_memory = None;
        let mut memory_seen = false;
        for input in &inputs.inputs {
            let one = self.stage_resolved_input(workspace, input, claim).await?;
            all.mounts.extend(one.mounts);
            all.prompts.extend(one.prompts);
            all.binding_checks.extend(one.binding_checks);
            all.repositories.extend(one.repositories);
            if let awaken_session_contract::ResolvedInputSource::MemoryStore {
                memory_store_id,
                config,
            } = &input.source
            {
                if memory_seen {
                    return Err(RunError::bad_request(
                        "automatic recall/extraction supports one MemoryStore binding per Session",
                    ));
                }
                memory_seen = true;
                let writable = input.access == awaken_resource_contract::ResourceAccess::ReadWrite;
                let materialization_reference =
                    all.mounts.iter().find_map(|mount| match &mount.source {
                        awaken_provisioning_contract::MountSource::MemoryStore {
                            store_id,
                            materialization_reference,
                            ..
                        } if store_id == memory_store_id.as_str() => {
                            materialization_reference.clone()
                        }
                        _ => None,
                    });
                let handle = self.host.platform_memory_handle(
                    materialization_reference
                        .clone()
                        .unwrap_or_else(|| memory_store_id.to_string()),
                    writable,
                );
                let resource_validator = if materialization_reference.is_some() {
                    None
                } else {
                    Some(self.resource_validator.as_ref().ok_or_else(|| {
                        RunError::bad_request(
                            "Memory extraction requires a configured resource binding validator",
                        )
                    })?.clone())
                };
                bound_memory = Some(Arc::new(self.host.memory.bind(
                    thread,
                    workspace,
                    handle,
                    resource_validator,
                    config,
                    writable,
                )));
            }
        }

        Ok((all, bound_memory))
    }

    async fn install_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        staged: crate::provisioning::StagedResources,
        bound_memory: Option<Arc<crate::memory::BoundMemory>>,
        update_authority_references: bool,
    ) -> Result<(), RunError> {
        // The complete manifest replaces the prior projection. Register an empty
        // value too, so deleting the final input cannot leave a stale mount behind.
        if update_authority_references {
            self.host
                .replace_session_references(workspace, thread, inputs)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?;
        }
        self.host.register_thread_resources(thread, staged);
        self.host.register_thread_resource_manifest(
            thread,
            awaken_session_contract::SessionResourceManifest::new(workspace, inputs.clone()),
        );
        // Every Session records an explicit selection (including none). There is
        // no Host-global or directory fallback.
        self.host.register_thread_memory(thread, bound_memory);
        if let Some(memory) = self.host.memory_for_thread(thread) {
            memory.reconcile(thread).await;
        }
        Ok(())
    }

    async fn stage_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        inputs: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
        update_authority_references: bool,
    ) -> Result<(), RunError> {
        let (staged, bound_memory) = self
            .compile_effective_inputs(thread, workspace, inputs, claim)
            .await?;
        self.install_effective_inputs(
            thread,
            workspace,
            inputs,
            staged,
            bound_memory,
            update_authority_references,
        )
        .await
    }

    /// Install one already-resolved Session resource manifest. This is shared by
    /// managed Session creation and cold durable workers; neither path reads Agent
    /// defaults or selects a newer mutable-resource configuration.
    async fn stage_resource_manifest(
        &self,
        thread: &str,
        workspace: &str,
        resources: &awaken_session_contract::ResolvedSessionResources,
        claim: Option<&awaken_run_ingress::RunClaim>,
        update_authority_references: bool,
    ) -> Result<(), RunError> {
        self.host.register_thread_workspace(thread, workspace);
        let desired =
            awaken_session_contract::SessionResourceManifest::new(workspace, resources.clone());
        // The authority-side recovery path may first reconcile a retained or
        // pending generation and then prepare the complete Session projection.
        // Both operations carry the same canonical manifest. Avoid compiling and
        // installing it twice; claimed Workers remain excluded because every
        // claim must revalidate live Resource state even when the pin is equal.
        if claim.is_none() && self.host.thread_resource_manifest(thread).as_ref() == Some(&desired)
        {
            return Ok(());
        }
        match &resources.skills {
            Some(bindings) => {
                let versions = self
                    .host
                    .skills
                    .load_pinned(workspace, bindings, claim)
                    .await
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                self.host
                    .session_slots
                    .update(thread, |slot| slot.skills = Some(versions));
            }
            None => {
                // Legacy records retain the historical global-catalog behavior;
                // modern `Some` manifests always install only their exact pins.
                self.host
                    .skills
                    .reload_cache_in(workspace)
                    .await
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                self.host
                    .session_slots
                    .update(thread, |slot| slot.skills = None);
            }
        }
        self.stage_effective_inputs(
            thread,
            workspace,
            resources,
            claim,
            update_authority_references,
        )
        .await
    }

    async fn validate_thread_resource_bindings(&self, thread: &str) -> Result<(), RunError> {
        use crate::provisioning::ResourceBindingCheck;

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
                    .validate_memory_binding(&workspace, &memory_store_id, config_version)
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
                    .map_err(|error| RunError::bad_request(error.to_string()))?,
            }
        }
        Ok(())
    }

    /// Wire runtime credential injection for the already-frozen Session bindings
    /// and Repository realization.
    #[must_use]
    pub fn with_credentials(
        self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    ) -> Self {
        self.with_credential_materializer(PinnedCredentialMaterializer::new(credentials, secrets))
    }

    /// Reuse the composition root's canonical exact materializer for MCP and
    /// Repository realization instead of constructing a peer over the same stores.
    #[must_use]
    pub fn with_credential_materializer(
        mut self,
        materializer: PinnedCredentialMaterializer,
    ) -> Self {
        self.credentials = Some(materializer);
        self.refresh_dispatch_session_runtime();
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
        self.refresh_dispatch_session_runtime();
        self
    }
}

#[async_trait::async_trait]
impl SessionRuntime for ManagedHost {
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

    async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.host.delegated_runs(thread).await.map_err(to_run_error)
    }

    async fn owns_thread(&self, thread: &str) -> bool {
        self.host.has_durable_thread(thread)
    }

    async fn end_session(&self, thread: &str) -> Result<(), RunError> {
        // Terminal release owns every reverse operation: publish Agent-authored Repo
        // commits (when the Agent did not own publication through MCP), persist
        // run-authored Skills, then dispose. A GET /files poll is never a write edge.
        self.host.publish_thread_repositories(thread).await;
        self.host.harvest_thread_skills(thread).await;
        // Failure is terminal-release blocking: keep the Sandbox available for
        // the durable cleanup retry instead of disposing unharvested outputs.
        self.harvest_artifacts(thread).await?;
        // Memory is owned by its MemoryMount guard: FUSE writes through live and
        // copy realization performs one CAS harvest during teardown.
        self.host.end_session(thread).await.map_err(to_run_error)
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
        // Same committed turn as `run`; `sink` mirrors in-flight `stream::Kind` so
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
            .resume(
                thread,
                tool_use_id,
                HostResume::ToolPermission {
                    allow: decision.allow,
                    note: decision.note,
                },
            )
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
            // The attempt closed between lookup and offer: same outcome as no
            // attempt at all.
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

    async fn add_system(&self, thread: &str, text: &str) -> Result<(), RunError> {
        self.host
            .add_system(thread, text)
            .await
            .map_err(to_run_error)
    }

    async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        managed_model_capability::supports_mid_conversation_system(
            &self.host.model_for_thread(thread),
        )
    }

    async fn pending_tool(&self, thread: &str) -> Option<Pending> {
        to_pending(self.host.pending_tool(thread).await)
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
    ) -> Result<OutcomeReport, RunError> {
        let report = self
            .host
            .define_outcome(thread, description, rubric, max_iterations)
            .await
            .map_err(to_run_error)?;
        Ok(OutcomeReport {
            iterations: report
                .iterations
                .into_iter()
                .map(|it| OutcomeIteration {
                    messages: it.messages,
                    outcome_id: it.outcome_id,
                    description: description.to_string(),
                    iteration: it.iteration,
                    result: it.result,
                    explanation: it.explanation,
                })
                .collect(),
        })
    }

    /// Replace only the already-selected model projection for one Session and
    /// force its next context construction to consume that exact selection.
    async fn rebind_model(&self, thread: &str, model: &str) -> Result<(), RunError> {
        // R5: re-stage the thread's model and evict its cached context so the next
        // turn rebuilds with the newly resolved executor (native switch is O(1); an
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
            .map_err(|error| RunError::bad_request(error.to_string()))
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        workspace_id: &str,
        inputs: &awaken_session_contract::ResolvedSessionResources,
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
        let desired_manifest =
            awaken_session_contract::SessionResourceManifest::new(workspace_id, inputs.clone());
        // Exact replays are the recovery/idempotency case, not a live mutation.
        // Return before the create-time-only Memory guard so concurrent/cold
        // Session rehydration cannot reject the already-installed generation.
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
            .inputs
            .iter()
            .filter_map(|input| match &input.source {
                awaken_session_contract::ResolvedInputSource::MemoryStore {
                    memory_store_id,
                    ..
                } => Some((
                    input.binding_id.to_string(),
                    memory_store_id.to_string(),
                    format!(".mnt/{}", input.mount_path.trim_start_matches('/')),
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
        let skill_versions = match &inputs.skills {
            Some(bindings) => Some(
                self.host
                    .skills
                    .load_pinned(workspace_id, bindings, None)
                    .await
                    .map_err(|error| RunError::bad_request(error.to_string()))?,
            ),
            None => None,
        };
        let (new, bound_memory) = self
            .compile_effective_inputs(thread, workspace_id, inputs, None)
            .await?;
        if let Some(environment) = &live_environment {
            environment
                .validate_live_mount_replacement(&old.mounts, &new.mounts)
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        }
        self.host.harvest_thread_skills(thread).await;
        self.host.publish_thread_repositories(thread).await;
        if let Some(environment) = &live_environment {
            // Realize the desired live projection before committing its logical
            // manifest. Every operation is idempotent, so a failed attempt leaves
            // the prior manifest authoritative and the persisted pending generation
            // can safely retry without mistaking an unrealized mount for success.
            // Delivered Skills are an exact, runtime-owned tree. Remove the old
            // projection at the manifest transition itself; the rebuilt context
            // materializes only the newly frozen versions. Authored `skills/`
            // remains independently owned and is untouched.
            environment
                .remove_workspace_path(crate::skills::DELIVERED_SKILLS_SUBDIR)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?;
            for mount in &old.mounts {
                if !new
                    .mounts
                    .iter()
                    .any(|candidate| candidate.mount_path == mount.mount_path)
                {
                    environment
                        .remove_workspace_path(&mount.mount_path)
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
                        .remove_workspace_path(&repository.plan.mount_path)
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
        self.install_effective_inputs(thread, workspace_id, inputs, new, bound_memory, true)
            .await?;
        self.host
            .session_slots
            .update(thread, |slot| slot.skills = skill_versions);
        self.host.evict_session_for_rebuild(thread).await;
        Ok(())
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
        // R1: retain the exact frozen Agent coordinate for internal cold-recovery
        // calls (`committed_messages`, durable operations) that intentionally do
        // not repeat a wire-level Agent argument.
        self.host
            .register_thread_agent_projection(thread, &init.agent_id);
        // R2: bind the session's requested model to the thread (independent of MCP),
        // consumed at the thread's first turn to resolve its executor + model name.
        if let Some(model) = &init.model {
            self.host.register_thread_model(thread, model);
        }
        // R3: cache the publication-derived backend carried by the frozen baseline.
        if let Some(backend_ref) = &init.runtime {
            self.host
                .register_thread_backend_projection(thread, backend_ref);
        }
        self.host
            .install_environment_projection(thread, &init.environment)
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
                // Session creation may have opened a durable ingress context
                // before its frozen Environment projection was prepared.
                slot.runtime = None;
            });
        }
        self.host
            .register_thread_delegates(thread, init.delegate_ids.clone());
        self.host
            .session_slots
            .update(thread, |slot| slot.toolsets = init.toolsets.clone());
        // Stage only the already-resolved manifest. Runtime never reads the Agent
        // binding repository or composes defaults again.
        self.stage_resource_manifest(thread, &init.workspace_id, &init.resources, None, true)
            .await?;
        Ok(())
    }

    async fn replace_session_toolsets(
        &self,
        thread: &str,
        toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    ) -> Result<(), RunError> {
        self.host
            .session_slots
            .update(thread, |slot| slot.toolsets = Some(toolsets));
        self.host.evict_session_for_rebuild(thread).await;
        Ok(())
    }

    async fn session_environment_binding(&self, thread: &str) -> Result<Option<String>, RunError> {
        self.host
            .session_environment_handle(thread)
            .await
            .map(|handle| {
                serde_json::to_string(&handle)
                    .map_err(|error| RunError::internal(error.to_string()))
            })
            .transpose()
    }

    async fn restore_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError> {
        let (adopted, rebuild) = self
            .host
            .adopt_bound_session_environment(thread, Some(binding), true)
            .await
            .map_err(to_run_error)?;
        // A graceful process shutdown disposes its physical sandbox while the
        // durable Session binding remains. Rebuild from committed Session truth;
        // ctx_for_with_sandbox persists the replacement binding before use.
        debug_assert!(adopted.is_none() || !rebuild);
        self.host
            .ctx_for_with_sandbox(thread, Some(agent), adopted)
            .await
            .map_err(to_run_error)?;
        Ok(())
    }

    /// Committed transcript from durable truth, so the adapter can rehydrate a
    /// session lost to a process restart and resume its awaiting run (ADR-0039).
    async fn committed_messages(&self, thread: &str) -> Vec<awaken_agent_contract::Message> {
        self.host.committed_messages(thread).await
    }

    async fn session_usage(&self, thread: &str) -> awaken_session_contract::SessionUsage {
        // Map the runtime's per-model tally onto the managed wire's session-level total
        // (the host is the context boundary; the managed crate never sees TokenUsage).
        let total = self.host.thread_usage(thread).await.total();
        awaken_session_contract::SessionUsage {
            input_tokens: total.prompt_tokens,
            output_tokens: total.completion_tokens,
            cache_read_tokens: total.cache_read_tokens,
            cache_creation_tokens: total.cache_creation_tokens,
        }
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

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for ManagedHost {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        use awaken_runtime_contract::{
            CredentialRealizationCapabilities, CredentialRealizationKind, PlaintextBoundary,
        };
        use std::collections::BTreeSet;

        if request.workspace_id.trim().is_empty()
            || request.generation.session_id.trim().is_empty()
            || request.realization_id.trim().is_empty()
            || request.stage_idempotency_key.trim().is_empty()
            || request.name.trim().is_empty()
            || request.target.url.trim().is_empty()
        {
            return Err(RunError::bad_request(
                "MCP realization request is incomplete",
            ));
        }
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        if !awaken_session_contract::realization_lease_is_live_at(
            request.generation.lease_expires_at_unix_ms,
            now_unix_ms,
        ) {
            return Err(RunError::classified(
                "mcp_stale_ownership",
                "MCP realization lease has expired",
            ));
        }
        let request_fingerprint = request.fingerprint();
        if let Some(existing) = self.host.mcp_projection(&request.generation) {
            if existing.realization_id == request.realization_id
                && existing.stage_idempotency_key == request.stage_idempotency_key
                && existing.receipt.receipt_fingerprint == request_fingerprint
                && existing.state != crate::session_slot::McpProjectionState::Removed
            {
                return Ok(existing.receipt);
            }
            return Err(RunError::classified(
                "mcp_stale_generation",
                "MCP generation is already bound to another realization",
            ));
        }
        match self.host.renew_mcp_projection(&request) {
            Ok(Some(receipt)) => return Ok(receipt),
            Ok(None) => {}
            Err(error) => {
                return Err(RunError::classified(
                    "mcp_stale_generation",
                    error.to_string(),
                ));
            }
        }

        let is_acp = self
            .host
            .session_slots
            .read(&request.generation.session_id, |slot| {
                slot.backend_ref.clone()
            })
            .flatten()
            .is_some_and(|backend_ref| {
                awaken_runtime_contract::resolved::Backend::from_ref(&backend_ref).is_acp()
            });

        let (bearer, refresh, actual_realization_kind) = match (
            request.credential.as_ref(),
            request.selected_plaintext_holder.as_ref(),
        ) {
            (None, None) => (None, None, None),
            (Some(access), Some(holder)) => {
                if holder.boundary != PlaintextBoundary::Worker {
                    return Err(RunError::classified(
                        "mcp_holder_unsupported",
                        "MCP Runtime supports only the Worker-held relay boundary",
                    ));
                }
                match &access.usage {
                    awaken_runtime_contract::CredentialUsage::HttpHeader { name, scheme }
                        if name.eq_ignore_ascii_case("authorization")
                            && scheme
                                .as_deref()
                                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer")) => {}
                    _ => {
                        return Err(RunError::classified(
                            "mcp_credential_usage_unsupported",
                            "MCP Runtime supports only canonical Authorization Bearer usage",
                        ));
                    }
                }
                if is_acp
                    && access.policy.model_exposure
                        != awaken_runtime_contract::ModelExposurePolicy::VirtualOnly
                {
                    return Err(RunError::classified(
                        "mcp_model_exposure_forbidden",
                        "authenticated ACP MCP requires explicit VirtualOnly authorization for its generation-scoped relay capability",
                    ));
                }
                if is_acp
                    && !self
                        .host
                        .session_provider
                        .capabilities()
                        .supports_secret_egress_without_bypass()
                {
                    return Err(RunError::classified(
                        "mcp_holder_unsupported",
                        "authenticated ACP MCP requires provider-enforced secret substitution and no-bypass networking before Worker relay materialization",
                    ));
                }
                let injector = self.credentials.as_ref().ok_or_else(|| {
                    RunError::classified(
                        "mcp_material_source_unavailable",
                        "MCP credential requires a configured material resolver",
                    )
                })?;
                let (material_sources, recipient_bound_envelopes) =
                    injector.material_source_capabilities();
                access
                    .admit(
                        holder,
                        CredentialRealizationKind::WorkerRelay,
                        &CredentialRealizationCapabilities {
                            holders: BTreeSet::from([holder.clone()]),
                            material_sources,
                            realization_kinds: BTreeSet::from([
                                CredentialRealizationKind::WorkerRelay,
                            ]),
                            recipient_bound_envelopes,
                            extension_consumers: Default::default(),
                            alternatives: Vec::new(),
                        },
                        now_unix_ms,
                    )
                    .map_err(|error| {
                        RunError::classified("mcp_credential_admission", error.to_string())
                    })?;
                let bearer = injector
                    .resolve_for_workspace(
                        access,
                        holder,
                        CredentialRealizationKind::WorkerRelay,
                        &request.workspace_id,
                        &(&request.name, &request.target),
                    )
                    .await
                    .map_err(|error| {
                        RunError::classified(
                            "mcp_credential_revision_mismatch",
                            format!(
                                "mcp server `{}` credential could not be resolved exactly: {error}",
                                request.name
                            ),
                        )
                    })?
                    .material
                    .into_secret()
                    .map_err(|error| {
                        RunError::classified(
                            "mcp_credential_material_kind_mismatch",
                            error.to_string(),
                        )
                    })?;
                let refresh = match access.refresh.as_ref() {
                    Some(refresh) => {
                        let (credentials, secrets) = injector.local_stores().ok_or_else(|| {
                            RunError::classified(
                                "mcp_credential_refresh_unavailable",
                                "MCP credential refresh requires a local credential authority",
                            )
                        })?;
                        Some(crate::mcp::McpRefreshMaterial::new(
                            awaken_credential_vault::CredentialSourceId(
                                access.credential.id.clone(),
                            ),
                            refresh.clone(),
                            credentials,
                            secrets,
                        ))
                    }
                    None => None,
                };
                (
                    Some(bearer),
                    refresh,
                    Some(CredentialRealizationKind::WorkerRelay),
                )
            }
            _ => {
                return Err(RunError::classified(
                    "mcp_credential_binding_invalid",
                    "MCP credential and selected plaintext holder must be present together",
                ));
            }
        };
        let renewal_binding_fingerprint = request.renewal_binding_fingerprint();
        let server = crate::mcp::McpTransportMaterial {
            name: request.name,
            url: request.target.url,
            prompts_as_skills: request.prompts_as_skills,
            bearer,
            refresh,
        };
        if is_acp && server.prompts_as_skills {
            return Err(RunError::classified(
                "mcp_prompt_skills_unsupported",
                "MCP prompts-as-skills requires the Native runtime; ACP does not expose a portable prompt-to-Skill projection",
            ));
        }
        let native_wiring = if is_acp {
            None
        } else {
            Some(
                crate::mcp::connect_materialized(std::slice::from_ref(&server))
                    .await
                    .map_err(to_run_error)?,
            )
        };
        // Staging is the sole route-creation boundary.  Runtime construction is
        // a projection reader and must never repair or recreate credential-
        // bearing effects behind the durable realization protocol's back.
        let staged_relay = if is_acp && server.bearer.is_some() {
            let relay = self
                .host
                .mcp_relay
                .get_or_try_init(crate::mcp_relay::McpRelay::start)
                .await
                .map_err(|error| {
                    RunError::classified(
                        "mcp_relay_unavailable",
                        format!("could not stage Worker-held MCP route: {error}"),
                    )
                })?;
            if !relay.stage_route(&request.generation, &server) {
                return Err(RunError::classified(
                    "mcp_stale_generation",
                    "MCP generation already has a staged relay route",
                ));
            }
            Some(relay)
        } else {
            None
        };
        let receipt = awaken_session_contract::McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind,
            receipt_fingerprint: request_fingerprint,
        };
        if let Err(error) =
            self.host
                .insert_mcp_projection(crate::session_slot::McpGenerationProjection {
                    generation: request.generation.clone(),
                    realization_id: request.realization_id,
                    stage_idempotency_key: request.stage_idempotency_key,
                    renewal_binding_fingerprint,
                    receipt: receipt.clone(),
                    server: Some(server),
                    native_wiring,
                    state: crate::session_slot::McpProjectionState::Staged,
                })
        {
            // A route is private and not yet visible, but retaining its bearer
            // after the exact projection failed to stage would still be a leak.
            if let Some(relay) = staged_relay {
                relay.remove_route(&request.generation);
            }
            return Err(to_run_error(error));
        }
        Ok(receipt)
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        if !awaken_session_contract::realization_lease_is_live_at(
            generation.lease_expires_at_unix_ms,
            now_unix_ms,
        ) {
            return Err(RunError::classified(
                "mcp_stale_ownership",
                "MCP publication lease has expired",
            ));
        }
        self.host
            .publish_mcp_projection(&generation)
            .await
            .map_err(to_run_error)
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.host
            .drain_mcp_projection(&generation)
            .await
            .map_err(to_run_error)
    }
}

impl ManagedHost {
    fn capabilities_for_workspace(&self, workspace: &str) -> AgentCapabilities {
        AgentCapabilities {
            builtin_tools: self
                .host
                .builtin_tools()
                .into_iter()
                .map(|(name, ask)| BuiltinTool { name, ask })
                .collect(),
            custom_tools: self
                .host
                .custom_tools()
                .into_iter()
                .map(|d| CustomTool {
                    name: d.id,
                    description: d.description,
                    input_schema: d.parameters,
                })
                .collect(),
            skills: self.host.skills.ids_in(workspace),
            delegates: self.host.delegate_ids_in(workspace),
        }
    }
}

// ── Protocol adapter over the shared host ───────────────────────────────────
//
// AG-UI, AI SDK, and A2A all drive one neutral `ProtocolRuntime` seam
// (`awaken-protocol-transport`), so a single host impl backs all three: a turn
// started through one protocol is resumable and observable through another on the
// same thread. Each wire adapter keeps only its own encoder + router.

fn to_driver_error(err: HostError) -> DriverError {
    match err.kind {
        HostErrorKind::BadRequest => DriverError::BadRequest(err.message),
        HostErrorKind::Internal => DriverError::Internal(err.message),
    }
}

fn to_port_pending(pending: Option<PendingTool>) -> Option<PortPending> {
    pending.map(|p| PortPending {
        tool_use_id: p.tool_use_id,
        name: p.name,
        input: p.input,
        client_executed: p.client_executed,
    })
}

fn to_port_step_outcome(result: RunResult) -> PortStepOutcome {
    // The run's terminal `RunState` maps to exactly one `Terminal`. A `RunState::Ended(Error)`
    // becomes `Terminal::Failed` — the neutral twin of `to_step_outcome`'s `PortStepFailure`
    // — so the wire adapter can surface a failed run (it is a successful `RunResult`,
    // not a `HostError`, so it never reaches the adapter as a `DriverError`).
    let terminal = match &result.state {
        RunState::Awaiting => Terminal::Awaiting {
            pending: to_port_pending(result.pending),
        },
        RunState::Ended(EndCause::MaxSteps) => Terminal::Exhausted,
        RunState::Ended(EndCause::Error(fault)) => Terminal::Failed(PortStepFailure {
            code: fault.code().to_string(),
            message: fault.message(),
        }),
        _ => Terminal::Finished,
    };
    PortStepOutcome {
        new_messages: result.new_messages,
        terminal,
    }
}

/// The neutral `ProtocolRuntime` port implemented once over the shared host and
/// wired behind every wire adapter (AI SDK / AG-UI / A2A) — a twin of
/// [`ManagedHost`]. All hold the same `Arc<SharedHost>`, so a turn started by one
/// protocol is resumable and observable through the others on the same thread.
pub struct ProtocolHost {
    host: Arc<SharedHost>,
    session_defaults: Option<Arc<dyn SessionDefaultsPreparer>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Session defaults could not be prepared: {0}")]
pub struct SessionDefaultsPreparationError(pub String);

#[async_trait::async_trait]
pub trait SessionDefaultsPreparer: Send + Sync {
    async fn prepare(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<(), SessionDefaultsPreparationError>;
}

impl ProtocolHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self {
            host,
            session_defaults: None,
        }
    }

    #[must_use]
    pub fn with_session_defaults(mut self, preparer: Arc<dyn SessionDefaultsPreparer>) -> Self {
        self.session_defaults = Some(preparer);
        self
    }

    async fn prepare_defaults(&self, thread: &str, agent: Option<&str>) -> Result<(), DriverError> {
        let Some(preparer) = &self.session_defaults else {
            return Ok(());
        };
        let projected_agent = self.host.thread_agent_projection(thread);
        preparer
            .prepare(
                &self.host.thread_workspace(thread),
                thread,
                agent.or(projected_agent.as_deref()).unwrap_or("assistant"),
            )
            .await
            .map_err(|error| DriverError::BadRequest(error.to_string()))
    }
}

#[async_trait::async_trait]
impl ProtocolRuntime for ProtocolHost {
    async fn run(
        &self,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<PortStepOutcome, DriverError> {
        self.prepare_defaults(thread, agent.as_deref()).await?;
        let result = self
            .host
            .run(agent.as_deref(), thread, messages)
            .await
            .map_err(to_driver_error)?;
        Ok(to_port_step_outcome(result))
    }

    async fn run_streaming(
        &self,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<PortStepOutcome, DriverError> {
        self.prepare_defaults(thread, agent.as_deref()).await?;
        let result = self
            .host
            .run_streaming(agent.as_deref(), thread, messages, sink)
            .await
            .map_err(to_driver_error)?;
        Ok(to_port_step_outcome(result))
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: PortResume,
    ) -> Result<PortStepOutcome, DriverError> {
        let resume = match resume {
            PortResume::Confirm { allow, note } => HostResume::ToolPermission { allow, note },
            PortResume::ClientResult { content, is_error } => {
                HostResume::ClientResult { content, is_error }
            }
        };
        let result = self
            .host
            .resume(thread, tool_use_id, resume)
            .await
            .map_err(to_driver_error)?;
        Ok(to_port_step_outcome(result))
    }

    async fn interrupt(&self, thread: &str) -> Result<(), DriverError> {
        // The same protocol-neutral cancel the managed `user.interrupt` uses: cancel
        // the in-flight run's token so an abandoned turn (dropped stream) stops.
        self.host.interrupt(thread).await.map_err(to_driver_error)
    }

    async fn pending(&self, thread: &str) -> Option<PortPending> {
        to_port_pending(self.host.pending_tool(thread).await)
    }

    async fn history(&self, thread: &str) -> Vec<Message> {
        self.host.committed_messages(thread).await
    }

    fn model(&self) -> String {
        self.host.model()
    }

    async fn usage(&self, thread: &str) -> (u64, u64) {
        let usage = self.host.thread_usage(thread).await.total();
        (usage.prompt_tokens, usage.completion_tokens)
    }
}
