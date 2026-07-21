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
mod background;
mod commit_backend;
mod commit_ingest;
mod compact;
mod config;
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
mod provisioning;
mod redact;
mod resource_ownership;
mod run_exec;
mod sandbox_source;
mod skill_catalog;
mod skills;
mod skills_api;
mod store;
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

mod worker_control_client;

// The neutral session substrate and its resume vocabulary.
pub use crate::commit_backend::init_shared_postgres_commit;
pub use crate::dispatch_backend::{
    ensure_durable_backend, init_shared_dispatch_store, init_shared_postgres_dispatch,
};
pub use crate::host::{HostResume, SharedHost};
pub use crate::worker_control_client::WorkerControlClient;
// The sandboxed ACP channel source (bwrap-confined agent launch) and the shared
// per-thread egress handle a composition root wires it with.
pub use crate::data_subject_api::{consent_router, erasure_router, install_capture_sink};
pub use crate::hub::{ThreadEvent, ThreadEventHub};
pub use crate::memory_store_api::{memory_stores_router, memory_stores_router_with_catalog};
pub use crate::redact::PiiRedactor;
pub use crate::sandbox_source::{
    AcpSandboxBindings, ContainerChannelSource, LaunchSource, SandboxChannelSource, ThreadEgress,
    ThreadResources, ThreadSandbox, build_acp_channel_source, resolve_sandbox_tier,
};
pub use crate::skills_api::skills_router;
// The config data plane (ADR-0036/slice A): the service + its router + the
// advertised-tools helper the composition root builds a config host from.
pub use crate::acp_provision::EnvLaunchResolver;
pub use crate::acp_serve::{AcpServeHost, AcpStop, AcpTurn};
pub use crate::binding_resolver::{
    AssistantBindingReconciler, ConfigServiceReconciler, ModelResolver, ResolvedModel,
};
pub use crate::capabilities::capabilities_router;
pub use crate::config::{
    advertised_tools, authorable_config_sections, block_text, platform_plugin_capabilities,
};
pub use crate::config_plane::{
    ConfigPlane, ConfigService, ConfigServiceAgentSource, PublishError, config_router,
};
pub use crate::tool_catalog::{
    RESERVED_ADMIN_SCOPE, ScopedToolCatalog, StaticToolCatalog, ToolCatalogSource,
};
// The per-plane resource routers the composition root merges over one host.
pub use crate::commit_ingest::{
    RemoteClaimedRunCommit, RemoteCoordinator, claimed_commit_ingest_router,
    claimed_commit_ingest_router_with_directory, commit_ingest_router,
};
pub use crate::deployment_config::{
    DeploymentConfig, DispatchBackend, SandboxTier, StoreKind, Wake,
};
pub use crate::dispatch_transport::{
    WorkerDispatchService, dispatch_transport_router, dispatch_transport_router_with_directory,
    dispatch_transport_router_with_directory_and_policy, dispatch_transport_router_with_service,
    registered_worker_transport_router, worker_dispatch_store_with_upstream,
};
pub use crate::worker_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, ManualWorkerClock, SystemWorkerClock,
    VerifiedWorkerContext, WORKER_ID_HEADER, WorkerAuthError, WorkerClock, WorkerLeasePolicy,
    WorkerRequestAuthenticator, WorkerUpstream,
};
// The worker HTTP dispatch client now lives in awaken-run-ingress; re-exported so
// composition roots keep using `awaken_runtime_host::{HttpDispatchQueue, worker_dispatch_store}`.
pub use crate::durable_ops::durable_ops_router;
pub use awaken_env_store::{PostgresEnvRegistry, SqliteEnvRegistry};
// The durable session-repository backends now live in `awaken-session-store` (a
// stores/ leaf); re-exported so composition roots keep their import paths.
pub use awaken_run_ingress::{HttpDispatchQueue, worker_dispatch_store};
pub use awaken_session_store::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};
// The durable WorkQueue backends now live in `awaken-work-store` (a stores/ leaf);
// re-exported so composition roots keep using `awaken_runtime_host::{Sqlite,Postgres}WorkQueue`.
pub use awaken_work_store::{PostgresWorkQueue, SqliteWorkQueue};
// The model-route seam (R1/R2/R5): a composition root supplies its own
// `InferenceExecutorMaterializer` to map a session's model ref to a labeled executor.
pub use crate::inference_routing::InferenceExecutorMaterializer;
// The managed-vault OAuth seams (ADR-0043): the transport-level refresher, its
// prepared configuration, and the live MCP credential probe.
pub use crate::mcp::{ExtMcpProbe, PreparedMcpRefresh, VaultRefresher};
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
    match err.kind {
        HostErrorKind::BadRequest => RunError::bad_request(err.message),
        HostErrorKind::Internal => RunError::internal(err.message),
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
/// Holds only an `Arc<SharedHost>` (plus, on the management server, the MCP
/// stores `prepare_session` reads), so it composes with any other adapter bound
/// to the same host.
pub struct ManagedHost {
    host: Arc<SharedHost>,
    credentials: Option<CredentialInjector>,
    mcp: Option<ManagedMcp>,
    resource_configs: Option<Arc<dyn awaken_resource_contract::ResourceConfigSource>>,
}

/// Runtime credential injection. This is independent of MCP and is also consumed
/// by Repository realization; persisted inputs carry references only.
#[derive(Clone)]
struct CredentialInjector {
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
}

/// The authored agent↔MCP configuration store. Credential materialization is
/// delegated to [`CredentialInjector`] instead of being owned by this component.
struct ManagedMcp {
    mcp_store: Arc<dyn awaken_config_resolver::McpStore>,
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
        Self {
            host,
            credentials: None,
            mcp: None,
            resource_configs: None,
        }
    }

    /// Wire the live resource-state/config read port used at activation. The
    /// pinned config stays authoritative; this lookup only supplies the current
    /// Workspace ownership and lifecycle deny overlay.
    #[must_use]
    pub fn with_resource_configs(
        mut self,
        source: Arc<dyn awaken_resource_contract::ResourceConfigSource>,
    ) -> Self {
        self.resource_configs = Some(source);
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
                if !self.host.owns_file(workspace, file_id.as_str()) {
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
                        // write-back path. Keep authorization (`input.access`) outside
                        // the sandbox protocol: a Workdir backend may let the agent
                        // alter its disposable copy without gaining mutation authority
                        // over the resource. Namespace/container adapters can still
                        // choose a read-only bind when they materialize by reference.
                        access: awaken_provisioning_contract::MountAccess::ReadWrite,
                        lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
                        required: true,
                    });
            }
            ResolvedInputSource::MemoryStore {
                memory_store_id, ..
            } => {
                let source = self.resource_configs.as_ref().ok_or_else(|| {
                    RunError::bad_request("memory resources require a configured Resource Catalog")
                })?;
                source
                    .resolve_memory_store(workspace, memory_store_id.as_str())
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                let mut versions = std::collections::BTreeMap::new();
                if input.access == ResourceAccess::ReadWrite {
                    for entry in self
                        .host
                        .memory_stores
                        .fs()
                        .list(memory_store_id.as_str(), "/")
                        .await
                        .map_err(|error| RunError::bad_request(error.to_string()))?
                    {
                        versions.insert(entry.path, entry.content_sha256);
                    }
                }
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
                if input.access == ResourceAccess::ReadWrite {
                    staged.memory_mounts.push(crate::provisioning::MemoryMount {
                        store_id: memory_store_id.to_string(),
                        logical: logical.clone(),
                        versions,
                    });
                }
            }
            ResolvedInputSource::Repository {
                repository_id,
                config,
            } => {
                let source = self.resource_configs.as_ref().ok_or_else(|| {
                    RunError::bad_request(
                        "repository resources require a configured Resource Catalog",
                    )
                })?;
                source
                    .resolve_repository(workspace, repository_id.as_str())
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                let credential = match &config.credential_binding {
                    Some(binding) => {
                        let credentials = self.credentials.as_ref().ok_or_else(|| {
                            RunError::bad_request(
                                "repository credential requires a configured credential vault",
                            )
                        })?;
                        let source_id =
                            awaken_credential_vault::CredentialSourceId(binding.clone());
                        let row =
                            credentials
                                .credentials
                                .get(&source_id)
                                .await
                                .map_err(|error| {
                                    RunError::bad_request(format!(
                                        "repository `{repository_id}` credential: {error}"
                                    ))
                                })?;
                        Some(
                            awaken_credential_vault::materialize(
                                &row,
                                credentials.secrets.as_ref(),
                            )
                            .await
                            .map_err(|error| {
                                RunError::bad_request(format!(
                                    "repository `{repository_id}` credential: {error}"
                                ))
                            })?,
                        )
                    }
                    None => None,
                };
                staged.repos.push(crate::provisioning::RepoStage {
                    logical,
                    url: config.remote_url.clone(),
                    git_ref: config.initial_branch.clone(),
                    credential,
                    access: mount_access,
                });
            }
        }
        Ok(staged)
    }

    /// Realize an already-resolved, secret-free manifest. The pinned Memory/
    /// Repository configuration in `inputs` remains authoritative; the per-item
    /// lookup in `stage_resolved_input` is only the current ownership/state deny
    /// overlay. No Agent binding or current config is composed here.
    async fn stage_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        inputs: &awaken_protocol_managed::EffectiveSessionInputs,
    ) -> Result<Vec<crate::host::PreparedMcpServer>, RunError> {
        let mut all = crate::provisioning::StagedResources::default();
        let mut bound_memory = None;
        let mut memory_seen = false;
        for input in &inputs.inputs {
            let one = self.stage_resolved_input(workspace, input).await?;
            all.mounts.extend(one.mounts);
            all.prompts.extend(one.prompts);
            all.memory_mounts.extend(one.memory_mounts);
            all.repos.extend(one.repos);
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
                if let Some(engine) = &self.host.memory {
                    let writable =
                        input.access == awaken_resource_contract::ResourceAccess::ReadWrite;
                    let handle = self
                        .host
                        .platform_memory_handle(memory_store_id.to_string(), writable);
                    bound_memory = Some(Arc::new(engine.for_binding(handle, config, writable)));
                }
            }
        }

        const GITHUB_MCP_URL: &str = "https://api.githubcopilot.com/mcp/";
        let repository_mcp = all
            .repos
            .iter()
            .filter(|repository| repository.credential.is_some())
            .map(|repository| crate::host::PreparedMcpServer {
                name: format!("github:{}", repository.logical),
                url: GITHUB_MCP_URL.to_string(),
                bearer: repository.credential.clone(),
                refresh: None,
            })
            .collect();
        // The complete manifest replaces the prior projection. Register an empty
        // value too, so deleting the final input cannot leave a stale mount behind.
        self.host.register_thread_resources(thread, all);
        // Managed sessions always record an explicit selection (including none),
        // preventing fallback to a host-global standalone memory directory.
        self.host.register_thread_memory(thread, bound_memory);
        Ok(repository_mcp)
    }

    /// Wire the MCP stores so `prepare_session` materializes a session's MCP
    /// credential bindings and merges the management plane's agent↔MCP config
    /// (ADR-0043 Phase 3). Hosts built without this keep the trait's no-op
    /// `prepare_session`, so no other server mode changes behavior.
    #[must_use]
    pub fn with_mcp(
        mut self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
        mcp_store: Arc<dyn awaken_config_resolver::McpStore>,
    ) -> Self {
        self.credentials = Some(CredentialInjector {
            credentials,
            secrets,
        });
        self.mcp = Some(ManagedMcp { mcp_store });
        self
    }

    /// Wire credential injection without enabling authored MCP configuration.
    /// Repository realization uses this seam directly.
    #[must_use]
    pub fn with_credentials(
        mut self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    ) -> Self {
        self.credentials = Some(CredentialInjector {
            credentials,
            secrets,
        });
        self
    }
}

/// A resolver credential lookup prefetched from the repo, scoped to exactly the
/// sources and pools the given MCP server defs' bindings name. The admin resolve
/// route builds its lookup by listing a workspace; the managed session ingress
/// has no workspace parameter on the wire, so it prefetches per binding instead —
/// same lookup shape, same fail-closed outcome (a missing source stays absent and
/// `resolve_mcp_servers` errors on it).
#[derive(Default)]
struct PrefetchedSourceLookup {
    sources: std::collections::HashMap<String, awaken_credential_vault::CredentialSource>,
    pools: std::collections::HashMap<String, awaken_credential_vault::CredentialPool>,
}

impl awaken_config_resolver::SourceLookup for PrefetchedSourceLookup {
    fn get(&self, id: &str) -> Option<&awaken_credential_vault::CredentialSource> {
        self.sources.get(id)
    }
    fn get_pool(&self, id: &str) -> Option<&awaken_credential_vault::CredentialPool> {
        self.pools.get(id)
    }
}

impl PrefetchedSourceLookup {
    /// Fetch every source/pool the defs' bindings reference. A row the repo does
    /// not hold is simply not inserted; resolution then fails closed on it.
    async fn for_defs(
        defs: &[awaken_config_resolver::McpServerDef],
        repo: &dyn awaken_credential_vault::repo::CredentialRepo,
    ) -> Self {
        use awaken_credential_vault::CredentialBinding;
        let mut lookup = Self::default();
        for def in defs {
            match &def.credential_binding {
                CredentialBinding::None => {}
                CredentialBinding::Exact {
                    credential_source_id,
                } => {
                    if let Ok(row) = repo.get(credential_source_id).await {
                        lookup.sources.insert(row.id.0.clone(), row);
                    }
                }
                CredentialBinding::OneOfCredentialPool { credential_pool_id } => {
                    if let Ok(pool) = repo.get_pool(credential_pool_id).await {
                        for member in &pool.members {
                            if let Ok(row) = repo.get(&member.credential_source_id).await {
                                lookup.sources.insert(row.id.0.clone(), row);
                            }
                        }
                        lookup.pools.insert(pool.id.0.clone(), pool);
                    }
                }
            }
        }
        lookup
    }
}

#[async_trait::async_trait]
impl SessionRuntime for ManagedHost {
    async fn owns_thread(&self, thread: &str) -> bool {
        self.host.has_durable_thread(thread)
    }

    async fn end_session(&self, thread: &str) -> Result<(), RunError> {
        // Terminal edge (managed session delete/archive): flush in-sandbox memory
        // edits and run-authored skills back to durable truth while the sandbox is
        // still live, then evict the cached context and dispose the sandbox (shred
        // secrets, reap the workspace). Mirrors the evict-rebuild harvest, but this
        // edge REAPS the workspace rather than reusing it — the session is over.
        self.host.harvest_thread_memory(thread).await;
        self.host.harvest_thread_skills(thread).await;
        self.host.end_session(thread).await.map_err(to_run_error)
    }

    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
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

    /// Provision a new session's MCP servers on its thread (ADR-0043 Phase 3),
    /// BEFORE the session record exists — a failure fails the create.
    ///
    /// 1. Each binding's vault credential is materialized to a bearer (a binding
    ///    without a credential stays bearer-less); a missing/broken credential
    ///    row is the caller's fault (`bad_request`, fail closed).
    /// 2. Management-plane merge: the agent's authored [`AgentMcpConfig`]
    ///    (admin `/v1/config/agents/{id}/mcp`), resolved through
    ///    `resolve_mcp_servers`, is appended AFTER the session-inline servers;
    ///    on a duplicate URL the session-inline server wins. A management-plane
    ///    config that cannot resolve is the deployment's fault (`internal`,
    ///    fail closed — the admin routes validated it at write time).
    /// 3. The prepared set is staged on the shared host; the thread's first turn
    ///    connects them (`SharedHost::register_thread_mcp` → `ctx_for`).
    ///
    /// Hosts built without [`ManagedHost::with_mcp`] keep the trait's no-op.
    ///
    /// [`AgentMcpConfig`]: awaken_config_resolver::AgentMcpConfig
    async fn rebind_model(&self, thread: &str, model: &str) -> Result<(), RunError> {
        // R5: re-stage the thread's model and evict its cached context so the next
        // turn rebuilds with the newly resolved executor (native switch is O(1); an
        // ACP thread's cached context relaunches its CLI on rebuild).
        self.host.register_thread_model(thread, model);
        self.host
            .evict_session_for_rebuild(thread, false)
            .await
            .map_err(to_run_error)
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        workspace_id: &str,
        inputs: &awaken_protocol_managed::EffectiveSessionInputs,
    ) -> Result<(), RunError> {
        self.host.register_thread_workspace(thread, workspace_id);
        self.host.harvest_thread_memory(thread).await;
        self.host.harvest_thread_skills(thread).await;
        self.host.harvest_thread_repo(thread).await;
        let repository_mcp = self
            .stage_effective_inputs(thread, workspace_id, inputs)
            .await?;
        self.host
            .replace_thread_repository_mcp(thread, repository_mcp);
        self.host
            .evict_session_for_rebuild(thread, true)
            .await
            .map_err(to_run_error)
    }

    async fn prepare_session(
        &self,
        thread: &str,
        init: awaken_protocol_managed::SessionInit,
    ) -> Result<(), RunError> {
        self.host
            .register_thread_workspace(thread, &init.workspace_id);
        self.host.skills.reload_cache_in(&init.workspace_id).await;
        // R2: bind the session's requested model to the thread (independent of MCP),
        // consumed at the thread's first turn to resolve its executor + model name.
        if let Some(model) = &init.model {
            self.host.register_thread_model(thread, model);
        }
        // R3: stage the session's runtime adapter; `acp:*` routes to the ACP CLI.
        if let Some(runtime) = &init.runtime {
            self.host.register_thread_runtime(thread, runtime);
        }
        // Stage the session's network-egress policy (from its environment): the first
        // turn's sandbox runs `bash` under `bwrap --unshare-net` when egress is denied.
        if init.deny_egress {
            self.host.register_thread_egress(thread, true);
        }
        // Stage the session's environment sandbox overlay: parse the raw `config.sandbox`
        // blob into a provisioning `SandboxOverride` HERE (the host owns the provisioning
        // contract; the neutral `SessionInit` carries only the blob). Consumed by
        // `sandbox_spec` (native jail) and the sandboxed ACP channel source, so a
        // UI-authored sandbox shapes both. A malformed blob contributes nothing.
        if let Some(over) = init
            .sandbox
            .as_ref()
            .and_then(awaken_provisioning_contract::SandboxOverride::from_config_value)
        {
            self.host.register_thread_sandbox(thread, over);
        }
        // Stage only the already-resolved manifest. Runtime never reads the Agent
        // binding repository or composes defaults again.
        let repo_mcp = self
            .stage_effective_inputs(thread, &init.workspace_id, &init.resources)
            .await?;
        let Some(credentials) = &self.credentials else {
            return Ok(());
        };
        let Some(mcp) = &self.mcp else {
            self.host.register_thread_mcp(thread, repo_mcp);
            return Ok(());
        };
        let mut prepared: Vec<crate::host::PreparedMcpServer> =
            Vec::with_capacity(init.mcp_servers.len());
        for binding in &init.mcp_servers {
            let (bearer, refresh) = match &binding.credential_source_id {
                Some(source_id) => {
                    // Re-type the port's neutral id string into the vault's domain id.
                    let source_id = awaken_credential_vault::CredentialSourceId(source_id.clone());
                    let row = credentials.credentials.get(&source_id).await.map_err(|e| {
                        RunError::bad_request(format!("mcp server `{}`: {e}", binding.name))
                    })?;
                    let bearer = awaken_credential_vault::materialize(&row, &*credentials.secrets)
                        .await
                        .map_err(|e| {
                            RunError::bad_request(format!("mcp server `{}`: {e}", binding.name))
                        })?;
                    // The binding's refresh configuration becomes a live
                    // refresher on the transport: it needs the row's
                    // material_ref to reseal the fresh access token (a vault
                    // row always has one; anything else cannot refresh).
                    let refresh = match (&binding.refresh, &row.material_ref) {
                        (Some(r), Some(access_token_ref)) => Some(crate::mcp::PreparedMcpRefresh {
                            token_endpoint: r.token_endpoint.clone(),
                            client_id: r.client_id.clone(),
                            token_endpoint_auth: r.token_endpoint_auth.clone(),
                            scope: r.scope.clone(),
                            resource: r.resource.clone(),
                            refresh_token_ref: awaken_credential_vault::SecretRef(
                                r.refresh_token_ref.clone(),
                            ),
                            access_token_ref: access_token_ref.clone(),
                            secrets: credentials.secrets.clone(),
                        }),
                        _ => None,
                    };
                    (Some(bearer), refresh)
                }
                None => (None, None),
            };
            prepared.push(crate::host::PreparedMcpServer {
                name: binding.name.clone(),
                url: binding.url.clone(),
                bearer,
                refresh,
            });
        }
        // The agent's authored workspace-level MCP binding. The aggregate carries
        // its owner, so execution verifies the config and every referenced server
        // against the already-trusted session workspace without a side projection.
        let authored = mcp
            .mcp_store
            .get_agent_config(&init.agent_id)
            .filter(|config| config.workspace_id == init.workspace_id)
            .map(|config| config.mcp_server_ids);
        if let Some(mcp_server_ids) = authored {
            let config_ids = mcp_server_ids;
            let mut defs = Vec::with_capacity(config_ids.len());
            for server_id in &config_ids {
                let def = mcp.mcp_store.get_server(&server_id.0).ok_or_else(|| {
                    RunError::internal(format!(
                        "agent `{}` references unknown mcp server `{}`",
                        init.agent_id, server_id.0
                    ))
                })?;
                if def.workspace_id != init.workspace_id {
                    return Err(RunError::internal(format!(
                        "agent `{}` references an mcp server outside its workspace",
                        init.agent_id
                    )));
                }
                defs.push(def);
            }
            let lookup = PrefetchedSourceLookup::for_defs(&defs, &*credentials.credentials).await;
            let resolved =
                awaken_config_resolver::resolve_mcp_servers(&defs, &lookup, &*credentials.secrets)
                    .await
                    .map_err(|e| {
                        RunError::internal(format!(
                            "agent `{}` mcp config did not resolve: {e}",
                            init.agent_id
                        ))
                    })?;
            for server in resolved {
                // Session-inline wins on a duplicate URL: the caller's explicit
                // request (and its vault binding) overrides the authored default.
                if prepared.iter().any(|p| p.url == server.url) {
                    continue;
                }
                prepared.push(crate::host::PreparedMcpServer {
                    name: server.name,
                    url: server.url,
                    bearer: server.credential,
                    // Management-plane servers resolve through the admin
                    // credential model, which has no OAuth refresh object.
                    refresh: None,
                });
            }
        }
        // Fold in the GitHub MCP servers bridged from github_repository resources, skipping a
        // name already staged (an explicit MCP binding of the same name wins).
        for server in repo_mcp {
            if prepared.iter().any(|p| p.name == server.name) {
                continue;
            }
            prepared.push(server);
        }
        self.host.register_thread_mcp(thread, prepared);
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
        self.capabilities_for_workspace(&workspace)
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
            delegates: self.host.delegate_ids(),
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
}

impl ProtocolHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self { host }
    }
}

#[async_trait::async_trait]
impl ProtocolRuntime for ProtocolHost {
    async fn run(
        &self,
        thread: &str,
        _agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<PortStepOutcome, DriverError> {
        let result = self
            .host
            .run(None, thread, messages)
            .await
            .map_err(to_driver_error)?;
        Ok(to_port_step_outcome(result))
    }

    async fn run_streaming(
        &self,
        thread: &str,
        _agent: Option<String>,
        messages: Vec<Message>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<PortStepOutcome, DriverError> {
        let result = self
            .host
            .run_streaming(None, thread, messages, sink)
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
