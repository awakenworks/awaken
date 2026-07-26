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
mod agent_catalog;
mod agent_runner;
mod application;
mod background;
mod commit_backend;
mod commit_ingest;
mod compact;
mod config;
mod container_environment;
mod credential_artifact;
mod credential_materializer;
mod data_subject_api;
mod delegate;
mod deployment_config;
mod dispatch_backend;
mod dispatch_transport;
mod durable_ops;
mod hand_placement;
mod host;
mod hub;
mod inference_routing;
mod judge;
mod live_inbox;
mod mcp;
mod mcp_relay;
mod memory;
mod memory_store_api;
mod memory_stores;
mod outcome_controller;
mod provisioning;
mod redact;
mod resource_reclamation;
mod resource_scope;
pub use resource_reclamation::HostResourceReclamation;
pub use resource_scope::RequiredWorkspaceScope;
mod run_exec;
mod sandbox_source;
mod session_environment;
mod session_slot;
pub use session_environment::HandExecutorFactory;
mod skill_catalog;
mod skills;
mod skills_api;
mod store;
#[cfg(test)]
mod test_mcp;
mod worker_http;
mod worker_security;

// The config-authoring plane now lives in the shared `awaken-config-service` crate
// (control ⊥ execution: `awaken-control` depends on it directly, not on this host).
// These thin aliases keep the historical `crate::{config_plane,binding_resolver,…}`
// module paths resolving for this host's internal consumers and the re-exports below.
use awaken_config_service as config_plane;
use awaken_config_service as binding_resolver;
use awaken_config_service as tool_catalog;
use awaken_config_service as capabilities;

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_protocol_managed::resource_plane as awaken_resource_contract;
use awaken_protocol_managed::{
    AgentCapabilities, BuiltinTool, CustomTool, LiveInboxEntry, LiveInboxError, LiveInboxSnapshot,
    OutcomeIteration, OutcomeReport, Pending, RunError, SessionRuntime, StepOutcome,
    ToolPermissionDecision,
};
use awaken_protocol_transport::{
    DriverError, Pending as PortPending, ProtocolRuntime, Resume as PortResume,
    StepFailure as PortStepFailure, StepOutcome as PortStepOutcome, Terminal,
};
use awaken_runtime_contract::live_inbox::{EditError, LiveInboxMessageId, MessageOrigin, Offer};

use crate::host::{HostError, HostErrorKind, PendingTool, RunResult};

mod postgres_migration_lock;
mod worker_control_client;

// The neutral session substrate and its resume vocabulary.
pub use crate::application::{
    ApplicationSessionControlClient, ApplicationSessionControlReceipt, ApplicationSessionError,
    ApplicationSessionPlan, ApplicationSessionProvisioner, WorkerControlApplicationSessionClient,
};
pub use crate::commit_backend::init_shared_postgres_commit;
pub use crate::credential_materializer::PinnedCredentialMaterializer;
pub use crate::dispatch_backend::init_shared_postgres_dispatch_with_config;
pub use crate::host::{
    AttemptExecutorDecorator, HostResume, ResourcePlanePorts, SharedHost,
    self_hosted_inference_holder,
};
pub use crate::postgres_migration_lock::PostgresMigrationLock;
pub use crate::worker_control_client::WorkerControlClient;
pub use awaken_protocol_managed::McpAttachmentRealizer;
pub use awaken_sandbox_container::{ContainerEnvironment, ContainerEnvironmentProvider};
// The sandboxed ACP channel source and the sole Session projection handle a
// composition root wires it with.
pub use crate::data_subject_api::{consent_router, erasure_router, install_capture_sink};
pub use crate::hub::{ThreadEvent, ThreadEventHub};
pub use crate::memory_store_api::memory_stores_router_with_catalog;
pub use crate::redact::PiiRedactor;
pub use crate::sandbox_source::{
    AcpLaunchRegistry, AcpSandboxBindings, BoundLocalChannelSource, LaunchSource,
    SandboxChannelSource, SessionRuntimeProjectionSource, build_acp_channel_source,
    resolve_sandbox_tier,
};
pub use crate::skills::SkillForkPlacement;
pub use crate::skills_api::skills_router;
// The config data plane (ADR-0036/slice A): the service + its router + the
// advertised-tools helper the composition root builds a config host from.
pub use crate::acp_provision::PublishedAcpLaunchResolver;
pub use crate::acp_serve::{AcpServeHost, AcpStop, AcpTurn};
pub use crate::binding_resolver::{
    AssistantBindingReconciler, ConfigServiceReconciler, ModelPublicationResolver,
    PublicationResolutionError, ResolvedPublicationModels,
};
pub use crate::capabilities::capabilities_router;
pub use crate::config::{
    advertised_tools, authorable_config_sections, authorable_tools, block_text,
    platform_plugin_capabilities,
};
pub use crate::config_plane::{
    ConfigPlane, ConfigService, ConfigServiceAgentSource, PublishError, config_router,
};
pub use crate::tool_catalog::{
    RESERVED_ADMIN_SCOPE, ScopedToolCatalog, StaticToolCatalog, ToolCatalogSource,
};
// The per-plane resource routers the composition root merges over one host.
pub use crate::commit_ingest::{
    ClaimedCommitService, RemoteClaimedRunCommit, claimed_commit_router,
};
pub use crate::deployment_config::{
    AcpWorkerProfile, DeploymentConfig, DispatchBackend, SandboxTier, StoreKind, Wake,
};
pub use crate::dispatch_transport::{
    WorkerDispatchService, dispatch_transport_router_with_service,
    registered_worker_transport_router, registered_worker_transport_router_with_services,
    worker_dispatch_store_with_upstream,
};
pub use crate::worker_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, ManualWorkerClock, MtlsWorkerAuthenticator,
    MtlsWorkerPrincipal, SIGNED_WORKER_SCHEME, SignedWorkerAuthenticator,
    SignedWorkerRequestAuthorizer, SystemWorkerClock, VerifiedWorkerContext, WORKER_ID_HEADER,
    WorkerAuthError, WorkerClock, WorkerCredentialError, WorkerLeasePolicy,
    WorkerRequestAuthenticator, WorkerSigningCredential, WorkerUpstream,
};
// The worker HTTP dispatch client now lives in awaken-run-ingress; re-exported so
// composition roots keep using the typed `awaken_runtime_host::HttpDispatchQueue`.
pub use crate::durable_ops::durable_ops_router;
pub use awaken_env_store::{PostgresEnvRegistry, SqliteEnvRegistry};
// The durable session-repository backends now live in `awaken-session-store` (a
// stores/ leaf); re-exported so composition roots keep their import paths.
pub use awaken_run_ingress::{
    HOST_EXECUTOR_CAPABILITY, HttpDispatchQueue, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY,
    WorkerRequestAuthorizer,
};
pub use awaken_session_store::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};

/// Select the local Managed Session repository from the runtime durability root.
/// Session configuration and its owner fence must survive whenever committed Run
/// facts survive; otherwise restart rehydration would recover execution without
/// recovering the authority that governs it.
pub fn local_managed_session_repository(
    storage_dir: Option<&std::path::Path>,
) -> Arc<dyn awaken_protocol_managed::ManagedSessionRepository> {
    let Some(dir) = storage_dir else {
        return Arc::new(
            SqliteManagedSessionRepository::open_in_memory()
                .expect("open ephemeral Managed Session repository"),
        );
    };
    std::fs::create_dir_all(dir).expect("create runtime storage directory");
    let path = dir.join("sessions.db");
    Arc::new(
        SqliteManagedSessionRepository::open(&path.to_string_lossy())
            .expect("open sessions.db under runtime storage directory"),
    )
}
// The durable WorkQueue backends now live in `awaken-work-store` (a stores/ leaf);
// re-exported so composition roots keep using `awaken_runtime_host::{Sqlite,Postgres}WorkQueue`.
pub use awaken_work_store::{PostgresWorkQueue, SqliteWorkQueue};
// The model-route seam (R1/R2/R5): a composition root supplies its own
// `InferenceExecutorMaterializer` to map a session's model ref to a labeled executor.
pub use crate::inference_routing::InferenceExecutorMaterializer;
// The managed-vault OAuth seams (ADR-0043): the transport-level refresher, its
// prepared configuration, and the live MCP credential probe.
pub use crate::mcp::{ExtMcpProbe, VaultRefresher};
// Skill authoring inputs (ADR-0036): a composition root supplies these to
// `SharedHost::with_skills`. The whole set is fronted by the single `Skill` tool.
pub use awaken_ext_skills::{SkillContext, SkillSpec, parse_skill_md};
pub use awaken_sandbox_local::content_fingerprint;
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
        MessageId(format!(
            "usr-{}",
            crate::host::BASE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        )),
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
/// Holds only an `Arc<SharedHost>` plus runtime-side materialization ports, so it
/// composes with any other adapter bound to the same host.
#[derive(Clone)]
pub struct ManagedHost {
    host: Arc<SharedHost>,
    credentials: Option<PinnedCredentialMaterializer>,
    resource_validator: Option<Arc<dyn awaken_resource_contract::ResourceBindingValidator>>,
    mcp_realizer: Option<Arc<dyn awaken_protocol_managed::McpAttachmentRealizer>>,
}

/// Weak, cloneable Worker-side adapter over the same configured Managed
/// `SessionRuntime`. Durable dispatch carries only secret-free projections; this
/// object reuses the installed Resource validator and credential ports for
/// Resource and MCP realization without constructing a parallel vault path.
#[derive(Clone)]
pub(crate) struct DispatchSessionRuntime {
    host: std::sync::Weak<SharedHost>,
    credentials: Option<PinnedCredentialMaterializer>,
    resource_validator: Option<Arc<dyn awaken_resource_contract::ResourceBindingValidator>>,
    mcp_realizer: Option<Arc<dyn awaken_protocol_managed::McpAttachmentRealizer>>,
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
            mcp_realizer: None,
        })
    }

    async fn install(
        &self,
        thread: &str,
        manifest: &awaken_protocol_managed::SessionResourceManifest,
    ) -> Result<(), RunError> {
        let managed = self.managed()?;
        let previous = managed.host.thread_resource_manifest(thread);
        if previous
            .as_ref()
            .is_some_and(|previous| previous != manifest)
        {
            awaken_protocol_managed::SessionRuntime::apply_session_inputs(
                &managed,
                thread,
                &manifest.workspace_id,
                &manifest.resources,
            )
            .await?;
        } else {
            // Re-stage even when the manifest is unchanged: ownership/lifecycle,
            // immutable File bytes, config-version integrity, and credential
            // revocation are live-deny checks at every claimed operation.
            managed
                .stage_resource_manifest(thread, &manifest.workspace_id, &manifest.resources)
                .await?;
        }
        Ok(())
    }

    async fn stage_mcp(
        &self,
        request: awaken_protocol_managed::StageMcpAttachment,
    ) -> Result<awaken_protocol_managed::McpRealizationReceipt, RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.stage_mcp_attachment(request).await,
            None => {
                awaken_protocol_managed::McpAttachmentRealizer::stage_mcp_attachment(
                    &self.managed()?,
                    request,
                )
                .await
            }
        }
    }

    async fn publish_mcp(
        &self,
        generation: awaken_protocol_managed::McpGenerationRef,
    ) -> Result<(), RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.publish_mcp_generation(generation).await,
            None => {
                awaken_protocol_managed::McpAttachmentRealizer::publish_mcp_generation(
                    &self.managed()?,
                    generation,
                )
                .await
            }
        }
    }

    async fn drain_mcp(
        &self,
        generation: awaken_protocol_managed::McpGenerationRef,
    ) -> Result<(), RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.drain_mcp_generation(generation).await,
            None => {
                awaken_protocol_managed::McpAttachmentRealizer::drain_mcp_generation(
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
        manifest: &awaken_protocol_managed::SessionResourceManifest,
    ) -> Result<(), RunError> {
        let preparer = self
            .dispatch_session_runtime
            .read()
            .expect("dispatch Session Runtime lock poisoned")
            .clone()
            .ok_or_else(|| {
                RunError::internal("durable resource dispatch has no Session Runtime")
            })?;
        preparer.install(thread, manifest).await
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
        request: awaken_protocol_managed::StageMcpAttachment,
    ) -> Result<awaken_protocol_managed::McpRealizationReceipt, RunError> {
        self.dispatch_session_runtime()?.stage_mcp(request).await
    }

    pub(crate) async fn publish_dispatched_mcp(
        &self,
        generation: awaken_protocol_managed::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?
            .publish_mcp(generation)
            .await
    }

    pub(crate) async fn drain_dispatched_mcp(
        &self,
        generation: awaken_protocol_managed::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?.drain_mcp(generation).await
    }
}

fn resolved_resource_prompt(input: &awaken_protocol_managed::ResolvedInput) -> String {
    use awaken_protocol_managed::ResolvedInputSource;
    use awaken_resource_contract::ResourceAccess;

    let access = match input.access {
        ResourceAccess::ReadOnly => "read-only",
        ResourceAccess::ReadWrite => "read/write",
    };
    let carried_path = format!(".mnt/{}", input.mount_path.trim_start_matches('/'));
    let base = match &input.source {
        ResolvedInputSource::File { .. } => {
            format!("A file is mounted read-only at `{carried_path}`.")
        }
        ResolvedInputSource::MemoryStore { .. } => {
            format!("A persistent memory store is mounted {access} at `{carried_path}`.")
        }
        ResolvedInputSource::Repository { .. } => format!(
            "A git repository is checked out at `{}` ({access}); use git there to read, edit, commit, and push.",
            input.mount_path
        ),
    };
    match &input.instructions {
        Some(instructions) if !instructions.is_empty() => format!("{base}\n{instructions}"),
        _ => base,
    }
}

impl ManagedHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        let managed = Self {
            host,
            credentials: None,
            resource_validator: None,
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
            mcp_realizer: self.mcp_realizer.clone(),
        });
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
        self.resource_validator = Some(validator);
        self.refresh_dispatch_session_runtime();
        self
    }

    async fn stage_resolved_input(
        &self,
        workspace: &str,
        input: &awaken_protocol_managed::ResolvedInput,
    ) -> Result<crate::provisioning::StagedResources, RunError> {
        use awaken_protocol_managed::ResolvedInputSource;
        use awaken_resource_contract::ResourceAccess;

        let mut staged = crate::provisioning::StagedResources::default();
        let logical = input.mount_path.trim_start_matches('/').to_string();
        staged.prompts.push(resolved_resource_prompt(input));
        let mount_access = match input.access {
            ResourceAccess::ReadOnly => awaken_provisioning_contract::MountAccess::ReadOnly,
            ResourceAccess::ReadWrite => awaken_provisioning_contract::MountAccess::ReadWrite,
        };

        match &input.source {
            ResolvedInputSource::File { file_id } => {
                if !self
                    .host
                    .owns_file(workspace, file_id.as_str())
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?
                {
                    return Err(RunError::bad_request(format!(
                        "file resource `{file_id}` not found in this workspace"
                    )));
                }
                let bytes = self
                    .host
                    .file_store()
                    .get(file_id.as_str())
                    .await
                    .ok()
                    .flatten()
                    .ok_or_else(|| {
                        RunError::bad_request(format!(
                            "file resource `{file_id}` not found in the blob store"
                        ))
                    })?;
                let actual = awaken_sandbox_local::content_fingerprint(&bytes);
                if actual != file_id.as_str() {
                    return Err(RunError::bad_request(format!(
                        "file resource `{file_id}` content hash mismatch (realized `{actual}`)"
                    )));
                }
                staged
                    .mounts
                    .push(awaken_provisioning_contract::MountRequirement {
                        mount_id: file_id.to_string(),
                        source: awaken_provisioning_contract::MountSource::InlineBytes {
                            contents: bytes,
                            content_hash: Some(file_id.to_string()),
                        },
                        mount_path: format!(".mnt/{logical}"),
                        // FileStore content is immutable and this per-run copy has no
                        // write-back path. Editing the disposable projection cannot
                        // mutate the File identified by `file_id`; publishing edited
                        // bytes creates a distinct File or Artifact.
                        access: awaken_provisioning_contract::MountAccess::ReadWrite,
                        lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
                        required: true,
                    });
                staged
                    .binding_checks
                    .push(crate::provisioning::ResourceBindingCheck::File {
                        file_id: file_id.to_string(),
                    });
            }
            ResolvedInputSource::MemoryStore {
                memory_store_id,
                config,
            } => {
                let validator = self.resource_validator.as_ref().ok_or_else(|| {
                    RunError::bad_request(
                        "memory resources require a configured resource binding validator",
                    )
                })?;
                validator
                    .validate_memory_binding(workspace, memory_store_id.as_str(), config.version)
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                staged.binding_checks.push(
                    crate::provisioning::ResourceBindingCheck::MemoryStore {
                        memory_store_id: memory_store_id.to_string(),
                        config_version: config.version,
                    },
                );
                // The worker realizes one governed store directory through its
                // MemoryMounter. The resource plane never receives a principal,
                // role, API key, or policy: the outer authorization/ACL seam has
                // already selected workspace, store, and maximum access.
                staged
                    .mounts
                    .push(awaken_provisioning_contract::MountRequirement {
                        mount_id: input.binding_id.to_string(),
                        source: awaken_provisioning_contract::MountSource::MemoryStore {
                            store_id: memory_store_id.to_string(),
                        },
                        mount_path: format!(".mnt/{logical}"),
                        access: mount_access,
                        lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
                        required: true,
                    });
            }
            ResolvedInputSource::Repository {
                repository_id,
                config,
                credential: credential_pin,
            } => {
                let validator = self.resource_validator.as_ref().ok_or_else(|| {
                    RunError::bad_request(
                        "repository resources require a configured resource binding validator",
                    )
                })?;
                validator
                    .validate_repository_binding(workspace, repository_id.as_str(), config.version)
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                staged
                    .binding_checks
                    .push(crate::provisioning::ResourceBindingCheck::Repository {
                        repository_id: repository_id.to_string(),
                        config_version: config.version,
                    });
                let credential = match (&config.credential_binding, credential_pin) {
                    (Some(binding), Some(pin)) => {
                        pin.validate_for_binding(binding).map_err(|error| {
                            RunError::bad_request(format!(
                                "repository `{repository_id}` credential: {error}"
                            ))
                        })?;
                        if pin.selected_plaintext_holder.boundary
                            != awaken_runtime_contract::PlaintextBoundary::Worker
                        {
                            return Err(RunError::bad_request(format!(
                                "repository `{repository_id}` credential requires an unsupported plaintext holder"
                            )));
                        }
                        let credentials = self.credentials.as_ref().ok_or_else(|| {
                            RunError::bad_request(
                                "repository credential requires a configured credential vault",
                            )
                        })?;
                        Some(
                            credentials
                                .resolve_for_workspace(
                                    &pin.access,
                                    &pin.selected_plaintext_holder,
                                    awaken_runtime_contract::CredentialRealizationKind::WorkerRelay,
                                    workspace,
                                    &(repository_id, config.version),
                                )
                                .await
                                .map_err(|error| {
                                    RunError::bad_request(format!(
                                        "repository `{repository_id}` credential: {error}"
                                    ))
                                })?
                                .material
                                .into_bearer()
                                .map_err(|error| {
                                    RunError::bad_request(format!(
                                        "repository `{repository_id}` credential: {error}"
                                    ))
                                })?,
                        )
                    }
                    (None, None) => None,
                    (Some(_), None) => {
                        return Err(RunError::bad_request(format!(
                            "repository `{repository_id}` credential binding has no exact Session pin"
                        )));
                    }
                    (None, Some(_)) => {
                        return Err(RunError::bad_request(format!(
                            "repository `{repository_id}` has a credential pin without a binding"
                        )));
                    }
                };
                staged
                    .repositories
                    .push(crate::provisioning::RepositoryActivation {
                        plan: awaken_provisioning_contract::RepositoryRealizationPlan {
                            repository_id: repository_id.to_string(),
                            mount_path: logical,
                            remote_url: config.remote_url.clone(),
                            initial_branch: config.initial_branch.clone(),
                            access: mount_access,
                        },
                        credential,
                    });
            }
        }
        Ok(staged)
    }

    /// Realize an already-resolved, secret-free manifest. The pinned Memory/
    /// Repository configuration in `inputs` remains authoritative; the per-item
    /// validation in `stage_resolved_input` checks only current ownership/state
    /// and the frozen version's integrity. No Agent binding or current config is
    /// composed here.
    async fn stage_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        inputs: &awaken_protocol_managed::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        let mut all = crate::provisioning::StagedResources::default();
        let mut bound_memory = None;
        let mut memory_seen = false;
        for input in &inputs.inputs {
            let one = self.stage_resolved_input(workspace, input).await?;
            all.mounts.extend(one.mounts);
            all.prompts.extend(one.prompts);
            all.binding_checks.extend(one.binding_checks);
            all.repositories.extend(one.repositories);
            if let awaken_protocol_managed::ResolvedInputSource::MemoryStore {
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
                let handle = self
                    .host
                    .platform_memory_handle(memory_store_id.to_string(), writable);
                let resource_validator = self.resource_validator.as_ref().ok_or_else(|| {
                    RunError::bad_request(
                        "Memory extraction requires a configured resource binding validator",
                    )
                })?;
                bound_memory = Some(Arc::new(self.host.memory.bind(
                    thread,
                    workspace,
                    handle,
                    resource_validator.clone(),
                    config,
                    writable,
                )));
            }
        }

        // The complete manifest replaces the prior projection. Register an empty
        // value too, so deleting the final input cannot leave a stale mount behind.
        self.host
            .replace_session_references(workspace, thread, inputs)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        self.host.register_thread_resources(thread, all);
        self.host.register_thread_resource_manifest(
            thread,
            awaken_protocol_managed::SessionResourceManifest::new(workspace, inputs.clone()),
        );
        // Every Session records an explicit selection (including none). There is
        // no Host-global or directory fallback.
        self.host.register_thread_memory(thread, bound_memory);
        if let Some(memory) = self.host.memory_for_thread(thread) {
            memory.reconcile(thread).await;
        }
        Ok(())
    }

    /// Install one already-resolved Session resource manifest. This is shared by
    /// managed Session creation and cold durable workers; neither path reads Agent
    /// defaults or selects a newer mutable-resource configuration.
    async fn stage_resource_manifest(
        &self,
        thread: &str,
        workspace: &str,
        resources: &awaken_protocol_managed::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        self.host.register_thread_workspace(thread, workspace);
        match &resources.skills {
            Some(bindings) => {
                let versions = self
                    .host
                    .skills
                    .load_pinned(workspace, bindings)
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
        self.stage_effective_inputs(thread, workspace, resources)
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
                ResourceBindingCheck::File { file_id } => {
                    if !self
                        .host
                        .owns_file(&workspace, &file_id)
                        .await
                        .map_err(|error| RunError::internal(error.to_string()))?
                    {
                        return Err(RunError::bad_request(format!(
                            "file resource `{file_id}` is unavailable in this Workspace"
                        )));
                    }
                }
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
                } => self
                    .resource_validator
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "repository resources require a configured resource binding validator",
                        )
                    })?
                    .validate_repository_binding(&workspace, &repository_id, config_version)
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
        realizer: Arc<dyn awaken_protocol_managed::McpAttachmentRealizer>,
    ) -> Self {
        self.mcp_realizer = Some(realizer);
        self.refresh_dispatch_session_runtime();
        self
    }
}

#[async_trait::async_trait]
impl SessionRuntime for ManagedHost {
    async fn owns_thread(&self, thread: &str) -> bool {
        self.host.has_durable_thread(thread)
    }

    async fn end_session(&self, thread: &str) -> Result<(), RunError> {
        // Terminal release owns every reverse operation: publish Agent-authored Repo
        // commits (when the Agent did not own publication through MCP), persist
        // run-authored Skills, then dispose. A GET /files poll is never a write edge.
        self.host.publish_thread_repositories(thread).await;
        self.host.harvest_thread_skills(thread).await;
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
            .await
            .map_err(to_run_error)?;
        to_step_outcome(result)
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
            .await
            .map_err(to_run_error)?;
        to_step_outcome(result)
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
            .await
            .map_err(to_run_error)?;
        to_step_outcome(result)
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        self.validate_thread_resource_bindings(thread).await?;
        let result = self
            .host
            .resume(
                thread,
                tool_use_id,
                HostResume::ClientResult {
                    content: content.to_string(),
                    is_error,
                },
            )
            .await
            .map_err(to_run_error)?;
        to_step_outcome(result)
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
        skill_ids: &[String],
    ) -> Result<Vec<awaken_protocol_managed::ResolvedSkillBinding>, RunError> {
        if skill_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.host
            .skills
            .resolve_latest(workspace_id, skill_ids)
            .await
            .map_err(|error| RunError::bad_request(error.to_string()))
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        workspace_id: &str,
        inputs: &awaken_protocol_managed::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        self.host.register_thread_workspace(thread, workspace_id);
        let old = self.host.thread_resources_snapshot(thread);
        let old_memory: Vec<_> = old
            .mounts
            .iter()
            .filter_map(|mount| match &mount.source {
                awaken_provisioning_contract::MountSource::MemoryStore { store_id } => Some((
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
                awaken_protocol_managed::ResolvedInputSource::MemoryStore {
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
        if live_environment.is_some() && old_memory != desired_memory {
            return Err(RunError::bad_request(
                "memory_store inputs are create-time only for a live Session",
            ));
        }
        self.host.harvest_thread_skills(thread).await;
        self.host.publish_thread_repositories(thread).await;
        match &inputs.skills {
            Some(bindings) => {
                let versions = self
                    .host
                    .skills
                    .load_pinned(workspace_id, bindings)
                    .await
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                self.host
                    .session_slots
                    .update(thread, |slot| slot.skills = Some(versions));
            }
            None => {
                self.host
                    .session_slots
                    .update(thread, |slot| slot.skills = None);
            }
        }
        self.stage_effective_inputs(thread, workspace_id, inputs)
            .await?;
        let new = self.host.thread_resources_snapshot(thread);
        if let Some(environment) = live_environment {
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
                if !old.mounts.iter().any(|candidate| candidate == mount)
                    && let awaken_provisioning_contract::MountSource::InlineBytes {
                        contents, ..
                    } = &mount.source
                {
                    environment
                        .materialize_workspace_file(&mount.mount_path, contents)
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
                        repository
                            .credential
                            .as_ref()
                            .map(|credential| credential.expose_secret()),
                    )
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?;
                }
            }
        }
        self.host.evict_session_for_rebuild(thread).await;
        Ok(())
    }

    async fn prepare_session(
        &self,
        thread: &str,
        init: awaken_protocol_managed::SessionInit,
    ) -> Result<(), RunError> {
        // R2: bind the session's requested model to the thread (independent of MCP),
        // consumed at the thread's first turn to resolve its executor + model name.
        if let Some(model) = &init.model {
            self.host.register_thread_model(thread, model);
        }
        // R3: stage the session's runtime adapter; `acp:*` routes to the ACP CLI.
        if let Some(runtime) = &init.runtime {
            self.host.register_thread_runtime(thread, runtime);
        }
        self.host
            .install_environment_projection(thread, &init.environment)
            .map_err(to_run_error)?;
        self.host
            .register_thread_delegates(thread, init.delegate_ids.clone());
        // Stage only the already-resolved manifest. Runtime never reads the Agent
        // binding repository or composes defaults again.
        self.stage_resource_manifest(thread, &init.workspace_id, &init.resources)
            .await?;
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
            .adopt_bound_session_environment(thread, Some(binding), false)
            .await
            .map_err(to_run_error)?;
        debug_assert!(
            !rebuild,
            "foreground restoration never requests replacement"
        );
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

    async fn session_usage(&self, thread: &str) -> awaken_protocol_managed::SessionUsage {
        // Map the runtime's per-model tally onto the managed wire's session-level total
        // (the host is the context boundary; the managed crate never sees TokenUsage).
        let total = self.host.thread_usage(thread).await.total();
        awaken_protocol_managed::SessionUsage {
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
impl awaken_protocol_managed::McpAttachmentRealizer for ManagedHost {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_protocol_managed::StageMcpAttachment,
    ) -> Result<awaken_protocol_managed::McpRealizationReceipt, RunError> {
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
        if !awaken_protocol_managed::realization_lease_is_live_at(
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
            .acp
            .as_ref()
            .and_then(|acp| acp.adapter_for(&request.generation.session_id))
            .is_some_and(|adapter| {
                awaken_runtime_contract::resolved::Backend::from_ref(&adapter).is_acp()
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
                    .into_bearer()
                    .map_err(|error| {
                        RunError::classified(
                            "mcp_credential_material_kind_mismatch",
                            error.to_string(),
                        )
                    })?;
                let refresh = access.refresh.as_ref().map(|refresh| {
                    crate::mcp::McpRefreshMaterial::new(refresh.clone(), injector.secret_store())
                });
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
            bearer,
            refresh,
        };
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
        let receipt = awaken_protocol_managed::McpRealizationReceipt {
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
        generation: awaken_protocol_managed::McpGenerationRef,
    ) -> Result<(), RunError> {
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        if !awaken_protocol_managed::realization_lease_is_live_at(
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
        generation: awaken_protocol_managed::McpGenerationRef,
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
        preparer
            .prepare(
                &self.host.thread_workspace(thread),
                thread,
                agent.unwrap_or("assistant"),
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
