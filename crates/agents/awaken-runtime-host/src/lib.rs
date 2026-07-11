//! `awaken-runtime-host` — the managed-agents SERVICE layer.
//!
//! It owns the protocol-neutral [`SharedHost`] (the thread-keyed session
//! substrate) and the two port adapters mounted over it: [`ManagedHost`] (the
//! Managed Agents `SessionRuntime`) and [`ProtocolHost`] (the neutral
//! `ProtocolRuntime` behind the AI SDK / AG-UI / A2A wire adapters). Both hold
//! the same `Arc<SharedHost>`, so a turn started through one protocol can be
//! resumed or observed through another on the *same thread*.
//!
//! The composition root (`awaken-server-local`) assembles these into routers;
//! this crate carries no wire assembly of its own beyond the per-plane routers
//! it exposes (config / files / memory-stores / durable-ops).

mod acp_backend;
mod acp_provision;
mod acp_serve;
mod agent_catalog;
mod background;
mod binding_resolver;
mod capabilities;
mod commit_backend;
mod compact;
mod config;
mod config_home;
mod config_plane;
mod data_subject_api;
mod delegate;
mod dispatch_backend;
mod durable_ops;
mod files;
mod host;
mod hub;
mod judge;
mod live_inbox;
mod mcp;
mod memory;
mod memory_store_api;
mod model_route;
mod models;
mod provisioning;
mod redact;
mod sandbox_source;
mod session_store;
mod skills;
mod skills_api;
mod store;
mod subagent;
mod tool_catalog;
mod turn_exec;

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Phase};
use awaken_protocol_managed::types::StopReason;
use awaken_protocol_managed::{
    AgentCapabilities, BuiltinTool, CustomTool, Decision, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, OutcomeIteration, OutcomeReport, Pending, RunError, SessionRuntime,
    TurnFailure, TurnOutcome,
};
use awaken_protocol_transport::{
    DriverError, Pending as PortPending, ProtocolRuntime, Resume as PortResume, StepOutcome,
};
use awaken_runtime_contract::live_inbox::{EditError, LiveInboxMessageId, MessageOrigin, Offer};

use crate::host::{HostError, HostErrorKind, PendingTool, TurnResult};

// The neutral session substrate and its resume vocabulary.
pub use crate::commit_backend::init_shared_postgres_commit;
pub use crate::dispatch_backend::{
    ensure_durable_backend, init_shared_dispatch_store, init_shared_postgres_dispatch,
};
pub use crate::host::{HostResume, SharedHost};
// The sandboxed ACP channel source (bwrap-confined agent launch) and the shared
// per-thread egress handle a composition root wires it with.
pub use crate::data_subject_api::{consent_router, erasure_router, install_capture_sink};
pub use crate::hub::{ThreadEvent, ThreadEventHub};
pub use crate::redact::PiiRedactor;
pub use crate::sandbox_source::{SandboxChannelSource, ThreadEgress};
// The config data plane (ADR-0036/slice A): the service + its router + the
// advertised-tools helper the composition root builds a config host from.
pub use crate::acp_provision::EnvLaunchResolver;
pub use crate::acp_serve::{AcpServeHost, AcpStop, AcpTurn};
pub use crate::binding_resolver::{
    AssistantBindingReconciler, ConfigServiceReconciler, ModelResolver, ResolvedModel,
};
pub use crate::capabilities::capabilities_router;
pub use crate::config::{advertised_tools, block_text};
pub use crate::config_home::{ConfigHome, RetentionPolicy, SessionReuse};
pub use crate::config_plane::{
    ConfigPlane, ConfigService, ConfigServiceAgentSource, PublishError, config_router,
};
pub use crate::tool_catalog::{
    RESERVED_ADMIN_SCOPE, ScopedToolCatalog, StaticToolCatalog, ToolCatalogSource,
};
// The per-plane resource routers the composition root merges over one host.
pub use crate::durable_ops::durable_ops_router;
pub use crate::files::files_router;
pub use crate::memory_store_api::memory_stores_router;
pub use crate::models::{ModelEntry, default_models, models_router};
pub use crate::session_store::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};
pub use crate::skills_api::skills_router;
// The model-route seam (R1/R2/R5): a composition root supplies its own
// `ExecutorProvider` to map a session's model ref to a labeled executor.
pub use crate::model_route::ExecutorProvider;
// The managed-vault OAuth seams (ADR-0043): the transport-level refresher, its
// prepared configuration, and the live MCP credential probe.
pub use crate::mcp::{ExtMcpProbe, PreparedMcpRefresh, VaultRefresher};
// Skill authoring inputs (ADR-0036): a composition root supplies these to
// `SharedHost::with_skills`. The whole set is fronted by the single `Skill` tool.
pub use awaken_ext_skills::{SkillContext, SkillSpec, parse_skill_md};
pub use awaken_sandbox_local::content_fingerprint;
// A remote delegate's transport belongs to the A2A bounded context; re-export it so
// a composition-root caller configures a remote agent from one import.
pub use awaken_protocol_a2a::{HttpTransport, Response, Transport};

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

/// Map a neutral terminal phase to the Managed idle `stop_reason`. `RequiresAction`
/// carries no event ids here; the projection refills them from the pending tool.
fn phase_to_stop(phase: &Phase) -> StopReason {
    match phase {
        Phase::Waiting => StopReason::RequiresAction {
            event_ids: Vec::new(),
        },
        // A step ceiling or a terminal fault both mean the run gave up rather than
        // ending naturally — `retries_exhausted` (the fault also projects a
        // `session.error`; the idle carries the exhausted stop reason).
        Phase::Ended(EndCause::MaxSteps | EndCause::Error(_)) => StopReason::RetriesExhausted,
        _ => StopReason::EndTurn,
    }
}

fn to_pending(pending: Option<PendingTool>) -> Option<Pending> {
    pending.map(|p| Pending {
        tool_use_id: p.tool_use_id,
        name: p.name,
        input: p.input,
        client_executed: p.client_executed,
    })
}

fn to_turn_outcome(result: TurnResult) -> TurnOutcome {
    // Carry a terminal fault through so the adapter projects `session.error`; the
    // neutral `Failure` owns the classification (code + message), not a string.
    let failure = match &result.phase {
        Phase::Ended(EndCause::Error(fault)) => Some(TurnFailure {
            code: fault.code().to_string(),
            message: fault.message(),
        }),
        _ => None,
    };
    TurnOutcome {
        stop: phase_to_stop(&result.phase),
        messages: result.new_messages,
        pending: to_pending(result.pending),
        compacted: result.compacted,
        failure,
    }
}

/// The Managed Agents `SessionRuntime` port implemented over the shared host.
/// Holds only an `Arc<SharedHost>` (plus, on the management server, the MCP
/// stores `prepare_session` reads), so it composes with any other adapter bound
/// to the same host.
pub struct ManagedHost {
    host: Arc<SharedHost>,
    mcp: Option<ManagedMcp>,
    /// The agent↔resource binding store (ADR-0038), shared with the config service.
    /// When wired, `prepare_session` also mounts the *agent's* bound resources — not
    /// just the session's wire `resources[]` — so a published agent's memory store is
    /// actually realized in the sandbox, closing the build→bind→use loop.
    resources: Option<Arc<dyn awaken_config_resolver::ResourceStore>>,
}

/// The session-ingress MCP wiring (ADR-0043 Phase 3): the stores
/// `prepare_session` reads to materialize a binding's vault credential and the
/// management plane's agent↔MCP config. Present only via [`ManagedHost::with_mcp`].
struct ManagedMcp {
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    mcp_store: Arc<dyn awaken_config_resolver::McpStore>,
}

/// The system-prompt fragment (ADR-0038 A3a) for a bound resource — the realized
/// mount path the agent reads (`.mnt/<logical>` for file/memory, the working-tree
/// path for a repo) plus access + instructions. Deterministic, so the live detach
/// path can reproduce and remove the exact fragment it staged.
fn resource_prompt(res: &awaken_protocol_managed::SessionResource) -> String {
    let logical = res.mount_path.trim_start_matches('/').to_string();
    let (kind, mount_path) = match res.kind.as_str() {
        "github_repository" => (
            awaken_config_resolver::ResourceKind::GithubRepository,
            logical,
        ),
        "memory_store" => (
            awaken_config_resolver::ResourceKind::MemoryStore,
            format!(".mnt/{logical}"),
        ),
        _ => (
            awaken_config_resolver::ResourceKind::File,
            format!(".mnt/{logical}"),
        ),
    };
    awaken_config_resolver::resource_binding_prompt(&awaken_config_resolver::ResourceBinding {
        kind,
        resource_id: res.id.clone(),
        mount_path,
        access: awaken_config_resolver::ResourceAccess::ReadWrite,
        instructions: res.instructions.clone(),
    })
}

/// Map an agent's bound resource (ADR-0038 [`ResourceBinding`]) to the neutral
/// [`SessionResource`] the sandbox stages — so a *published agent's* bindings mount
/// exactly the way a session's wire `resources[]` do. Access isn't carried on the wire
/// type (a memory mount realizes read-write; a read-only binding is advisory via its
/// already-compiled prompt), and a private resource's credential stays a vault
/// reference, never material here.
///
/// [`ResourceBinding`]: awaken_config_resolver::ResourceBinding
fn binding_as_session_resource(
    b: &awaken_config_resolver::ResourceBinding,
) -> awaken_protocol_managed::SessionResource {
    use awaken_config_resolver::ResourceKind as K;
    let kind = match b.kind {
        K::MemoryStore => "memory_store",
        K::File => "file",
        K::GithubRepository => "github_repository",
        K::Skill => "skill",
        K::Outputs => "outputs",
    };
    awaken_protocol_managed::SessionResource {
        kind: kind.to_string(),
        id: b.resource_id.clone(),
        mount_path: b.mount_path.clone(),
        instructions: b.instructions.clone(),
        auth_token: None,
        git_ref: None,
    }
}

impl ManagedHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self {
            host,
            mcp: None,
            resources: None,
        }
    }

    /// Share the agent↔resource binding store (ADR-0038) so `prepare_session` mounts
    /// a published agent's bound resources (memory stores, files) — not only the
    /// session's wire `resources[]`. The same store instance backs the config service's
    /// prompt injection, so what the agent is *told* it has is what actually gets mounted.
    #[must_use]
    pub fn with_resources(
        mut self,
        resources: Arc<dyn awaken_config_resolver::ResourceStore>,
    ) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Stage ONE resource (ADR-0038) into a partial [`StagedResources`]: resolve its
    /// seed bytes and realize it as a sandbox mount + prompt fragment (+ memory
    /// write-back tracking / repo clone stage). Shared by create-time
    /// `prepare_session` (folded over all resources) and the live `attach_resource`
    /// path. A `file`/`memory_store` whose backing store is missing fails closed.
    async fn stage_one_resource(
        &self,
        res: &awaken_protocol_managed::SessionResource,
    ) -> Result<crate::provisioning::StagedResources, RunError> {
        use awaken_sandbox_local::{Mount, ResourceMount};
        let mut staged = crate::provisioning::StagedResources::default();
        let logical = res.mount_path.trim_start_matches('/').to_string();
        // github_repository is a host-side `git clone` (ADR-0038), not a byte mount —
        // the token authenticates the clone transport host-side and never enters the jail.
        if res.kind == "github_repository" {
            staged.prompts.push(resource_prompt(res));
            staged.repos.push(crate::provisioning::RepoStage {
                logical,
                url: res.id.clone(),
                git_ref: res.git_ref.clone(),
                token: res
                    .auth_token
                    .clone()
                    .map(awaken_agent_contract::RedactedString::from),
            });
            return Ok(staged);
        }
        if res.kind != "file" && res.kind != "memory_store" {
            return Ok(staged);
        }
        staged.prompts.push(resource_prompt(res));
        // Resolve seed content by family: a file from the content-addressed blob store,
        // a memory_store from its mutable id-keyed store (tracked for write-back).
        let content = match res.kind.as_str() {
            "file" => match self.host.file_store().get(&res.id).await {
                Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
                _ => {
                    return Err(RunError::bad_request(format!(
                        "file resource `{}` not found in the blob store",
                        res.id
                    )));
                }
            },
            _ => {
                let Some(bytes) = self.host.memory_get(&res.id) else {
                    return Err(RunError::bad_request(format!(
                        "memory_store resource `{}` does not exist",
                        res.id
                    )));
                };
                staged.memory_mounts.push((res.id.clone(), logical.clone()));
                String::from_utf8_lossy(&bytes).into_owned()
            }
        };
        staged.mounts.push(Mount::Resource(ResourceMount {
            id: res.id.clone(),
            content_hash: String::new(),
            logical_path: logical,
            content,
        }));
        Ok(staged)
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
        self.mcp = Some(ManagedMcp {
            credentials,
            secrets,
            mcp_store,
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

    async fn run_turn(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError> {
        let result = self
            .host
            .run_turn(Some(agent), thread, vec![user_message(content)])
            .await
            .map_err(to_run_error)?;
        Ok(to_turn_outcome(result))
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: Decision,
    ) -> Result<TurnOutcome, RunError> {
        let result = self
            .host
            .resume(
                thread,
                tool_use_id,
                HostResume::Confirm {
                    allow: decision.allow,
                    note: decision.note,
                },
            )
            .await
            .map_err(to_run_error)?;
        Ok(to_turn_outcome(result))
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<TurnOutcome, RunError> {
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
        Ok(to_turn_outcome(result))
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
        self.host.sessions.lock().await.remove(thread);
        Ok(())
    }

    async fn attach_resource(
        &self,
        thread: &str,
        resource: awaken_protocol_managed::SessionResource,
    ) -> Result<(), RunError> {
        // Flush in-sandbox memory edits before dropping the cached sandbox, then stage
        // the new resource (fail closed on a missing backing store) and evict so the
        // next turn rebuilds WITH it.
        self.host.harvest_thread_memory(thread).await;
        let one = self.stage_one_resource(&resource).await?;
        self.host.merge_thread_resources(thread, one);
        self.host.sessions.lock().await.remove(thread);
        Ok(())
    }

    async fn detach_resource(
        &self,
        thread: &str,
        resource: awaken_protocol_managed::SessionResource,
    ) -> Result<(), RunError> {
        // Flush write-back while the old sandbox is still live (preserve edits to other
        // still-mounted memory stores), drop this resource's mount + prompt, then evict
        // so the next turn rebuilds WITHOUT it.
        self.host.harvest_thread_memory(thread).await;
        let logical = resource.mount_path.trim_start_matches('/').to_string();
        self.host
            .remove_thread_resource(thread, &logical, &resource_prompt(&resource));
        self.host.sessions.lock().await.remove(thread);
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
        // Stage the session's network-egress policy (from its environment): the first
        // turn's sandbox runs `bash` under `bwrap --unshare-net` when egress is denied.
        if init.deny_egress {
            self.host.register_thread_egress(thread, true);
        }
        // Stage the effective resource set (ADR-0038): the session's wire `resources[]`
        // PLUS the *agent's* bound resources when the binding store is shared. Both
        // resolve to a sandbox mount via `stage_one_resource`, folded and registered
        // (replace — correct at create, before the first turn). Independent of MCP, so
        // it runs before the MCP gate.
        let mut all = crate::provisioning::StagedResources::default();
        // Wire session resources carry their own prompt (no compile-time fragment
        // exists for an ad-hoc session mount), so we keep `one.prompts`.
        for res in &init.resources {
            let one = self.stage_one_resource(res).await?;
            all.mounts.extend(one.mounts);
            all.prompts.extend(one.prompts);
            all.memory_mounts.extend(one.memory_mounts);
            all.repos.extend(one.repos);
        }
        // Agent-bound resources: MOUNT only. The prompt fragment is already in the
        // compiled system prompt (config service `resource_prompts`), so re-staging it
        // would duplicate the description — we drop `one.prompts` and keep the mount.
        // This is the seam that actually realizes a published agent's memory store:
        // config injects the prompt, this injects the mount. Skip a mount path the
        // wire set already claimed (an explicit per-session override wins).
        if let Some(store) = &self.resources {
            if let Some(cfg) = store.get_agent_resource(&init.agent_id) {
                let taken: std::collections::HashSet<String> = init
                    .resources
                    .iter()
                    .map(|r| r.mount_path.trim_start_matches('/').to_string())
                    .collect();
                for b in &cfg.resources {
                    if taken.contains(b.mount_path.trim_start_matches('/')) {
                        continue;
                    }
                    let res = binding_as_session_resource(b);
                    let one = self.stage_one_resource(&res).await?;
                    all.mounts.extend(one.mounts);
                    all.memory_mounts.extend(one.memory_mounts);
                    all.repos.extend(one.repos);
                }
            }
        }
        if !all.mounts.is_empty()
            || !all.prompts.is_empty()
            || !all.repos.is_empty()
            || !all.memory_mounts.is_empty()
        {
            self.host.register_thread_resources(thread, all);
        }
        let Some(mcp) = &self.mcp else {
            return Ok(());
        };
        let mut prepared: Vec<crate::host::PreparedMcpServer> =
            Vec::with_capacity(init.mcp_servers.len());
        for binding in &init.mcp_servers {
            let (bearer, refresh) = match &binding.credential_source_id {
                Some(source_id) => {
                    let row = mcp.credentials.get(source_id).await.map_err(|e| {
                        RunError::bad_request(format!("mcp server `{}`: {e}", binding.name))
                    })?;
                    let bearer = awaken_credential_vault::materialize(&row, &*mcp.secrets)
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
                            refresh_token_ref: r.refresh_token_ref.clone(),
                            access_token_ref: access_token_ref.clone(),
                            secrets: mcp.secrets.clone(),
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
        // The agent's authored workspace-level MCP binding (tenancy-agnostic core:
        // MCP selection is by agent id, not by any tenancy tier).
        let authored = mcp
            .mcp_store
            .get_agent_config(&init.agent_id)
            .map(|c| c.mcp_server_ids);
        if let Some(mcp_server_ids) = authored {
            let config_ids = mcp_server_ids;
            let mut defs = Vec::with_capacity(config_ids.len());
            for server_id in &config_ids {
                defs.push(mcp.mcp_store.get_server(&server_id.0).ok_or_else(|| {
                    RunError::internal(format!(
                        "agent `{}` references unknown mcp server `{}`",
                        init.agent_id, server_id.0
                    ))
                })?);
            }
            let lookup = PrefetchedSourceLookup::for_defs(&defs, &*mcp.credentials).await;
            let resolved =
                awaken_config_resolver::resolve_mcp_servers(&defs, &lookup, &*mcp.secrets)
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
        self.host.register_thread_mcp(thread, prepared);
        Ok(())
    }

    /// Committed transcript from durable truth, so the adapter can rehydrate a
    /// session lost to a process restart and resume its parked run (ADR-0039).
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
            skills: self.host.skill_ids(),
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

fn to_step_outcome(result: TurnResult) -> StepOutcome {
    StepOutcome {
        waiting: matches!(result.phase, Phase::Waiting),
        exhausted: matches!(result.phase, Phase::Ended(EndCause::MaxSteps)),
        new_messages: result.new_messages,
        pending: to_port_pending(result.pending),
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
    async fn run_turn(
        &self,
        thread: &str,
        _agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        let result = self
            .host
            .run_turn(None, thread, messages)
            .await
            .map_err(to_driver_error)?;
        Ok(to_step_outcome(result))
    }

    async fn run_turn_streaming(
        &self,
        thread: &str,
        _agent: Option<String>,
        messages: Vec<Message>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, DriverError> {
        let result = self
            .host
            .run_turn_streaming(None, thread, messages, sink)
            .await
            .map_err(to_driver_error)?;
        Ok(to_step_outcome(result))
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: PortResume,
    ) -> Result<StepOutcome, DriverError> {
        let resume = match resume {
            PortResume::Confirm { allow, note } => HostResume::Confirm { allow, note },
            PortResume::ClientResult { content, is_error } => {
                HostResume::ClientResult { content, is_error }
            }
        };
        let result = self
            .host
            .resume(thread, tool_use_id, resume)
            .await
            .map_err(to_driver_error)?;
        Ok(to_step_outcome(result))
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
