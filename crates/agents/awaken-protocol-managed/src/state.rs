//! Adapter state: the session store, id minting, and the `SessionRuntime` port.
//!
//! The adapter drives one runtime seam and owns no kernel construction. The
//! server implements [`SessionRuntime`] over the runtime; tests implement it with
//! a fake. Public ids (`sesn_*`, `evt_*`) are minted here; a tool-use event keeps
//! the tool call's own id so a `user.tool_confirmation` can reference it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_credential_vault::CredentialSourceId;

use crate::ext::AwakenModelSelection;
use crate::project::{self, project_messages, project_turn};
use crate::routes::vaults::{McpRefreshBinding, VaultState};
use crate::session_repo::{InMemorySessionRepository, ManagedSessionRepository, PersistedSession};
use crate::types::{
    ConfirmResult, Event, EventReceipt, InboundEvent, ListEventsResponse, ModelConfig,
    OutboundKind, SendEventsRequest, SendEventsResponse, Session, SessionAgent,
    SessionCreateParams, SessionError, SessionStats, StopReason, Usage,
};

/// The seeded owner scope a bare/self-hosted session is created under when the
/// edge resolved no workspace (ADR-0051 / ADR-0048 D2 "seeded, not absent"). It
/// matches the request scope the ownership guard derives for an unscoped request,
/// so a single-tenant deployment never 404s itself.
pub(crate) const DEFAULT_SCOPE: &str = "default";

/// A fixed projection timestamp (M1). Real per-event timestamps arrive with a
/// clock port; the wire only needs a valid RFC 3339 value here.
const PROCESSED_AT: &str = "2026-01-01T00:00:00Z";

/// The Managed Agents contract error for a `memory_store` add/remove on a running
/// session — memory stores bind at session creation only.
const MEMORY_CREATE_ONLY: &str = "memory stores can only be attached at session creation time; \
     adding or removing one from a running session is not supported";

/// The tool a run parked on: its id, model-visible name/input, and whether it is
/// client-executed (projected as `agent.custom_tool_use`) or a built-in awaiting
/// confirmation (`agent.tool_use{ask}`).
pub struct Pending {
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub client_executed: bool,
}

/// The result of running one step (a new turn, or a resume). `pending` is set when
/// `stop` is `RequiresAction`.
pub struct TurnOutcome {
    pub messages: Vec<Message>,
    pub stop: StopReason,
    pub pending: Option<Pending>,
    /// `true` when this turn folded its context — projected as an
    /// `agent.thread_context_compacted` event ahead of the turn's messages.
    pub compacted: bool,
    /// Set when the run ended in a terminal fault (the neutral `EndCause::Error`) —
    /// projected as a `session.error` event before the turn goes idle, so a client
    /// observes the failure. `None` on a normal completion.
    pub failure: Option<TurnFailure>,
}

/// A terminal run fault carried from the neutral `EndCause::Error` so the adapter
/// can project `session.error`. Neutral (a stable `code` + human `message`), not
/// managed-wire vocabulary.
pub struct TurnFailure {
    pub code: String,
    pub message: String,
}

/// A human-in-the-loop tool decision, delivered by `user.tool_confirmation`.
pub struct Decision {
    pub allow: bool,
    pub note: Option<String>,
}

/// The advertised capability surface echoed in a session's agent object. The adapter
/// reads this once at session creation so the public agent object reports what the run
/// can actually do. This is neutral data; the public Managed Agents wire shaping (the
/// built-in `agent_toolset` fold, `custom` tools, `skills`, `multiagent`) lives in
/// [`crate::project`]. Deliberately absent: MCP servers (the host wires none) and
/// session resources (the host has no Files-API-backed resource to reference yet), so
/// those wire fields stay empty until a real producer exists.
#[derive(Default)]
pub struct AgentCapabilities {
    /// The registered built-in tools (the hand toolset). Each names a tool of the
    /// versioned agent toolset and whether its calls require human confirmation.
    pub builtin_tools: Vec<BuiltinTool>,
    /// Client-executed tools: the caller runs them and returns the result.
    pub custom_tools: Vec<CustomTool>,
    /// Skills the agent offers (activated on demand, not model-visible as tools).
    pub skills: Vec<String>,
    /// Delegate agents the agent may coordinate (the multiagent roster).
    pub delegates: Vec<String>,
}

/// One registered built-in tool: its name and whether calls require confirmation
/// (`ask` = the permission gate parks the call for an approval).
pub struct BuiltinTool {
    pub name: String,
    pub ask: bool,
}

/// One client-executed custom tool: the model-visible name, description, and input
/// schema the runtime pins for it.
pub struct CustomTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// One evaluation round of a goal: the agent's revision messages committed this
/// round (empty when grading the existing deliverable), and the verdict.
pub struct OutcomeIteration {
    pub messages: Vec<Message>,
    pub outcome_id: String,
    pub iteration: u32,
    pub result: String,
    pub explanation: String,
}

/// The result of `user.define_outcome`: the ordered evaluation rounds. The loop
/// always ends idle (`end_turn`).
pub struct OutcomeReport {
    pub iterations: Vec<OutcomeIteration>,
}

/// What a new session provisions on its thread before the first turn (ADR-0043
/// Phase 3): the agent it runs and the MCP servers it connects to, each already
/// bound to a vault credential's neutral domain id (or none). Consumed by the
/// server's `ManagedHost` through [`SessionRuntime::prepare_session`].
pub struct SessionInit {
    pub agent_id: String,
    pub mcp_servers: Vec<McpServerBinding>,
    /// The session's mounted resources (ADR-0038), parsed from the wire `resources[]`:
    /// files, memory stores, repos. The host realizes each into the run's sandbox and
    /// appends a prompt fragment to the system prompt (A3a). Empty = no mounts.
    pub resources: Vec<SessionResource>,
    /// The session's requested model (R2), staged so the run binds it; `None` →
    /// the host default.
    pub model: Option<String>,
    /// The session's requested runtime adapter (R3): `"acp:*"` routes to an ACP
    /// CLI; `None`/`"awaken"` → native.
    pub runtime: Option<String>,
    /// Deny network egress for the session's sandbox, resolved from its environment's
    /// networking policy (a non-`unrestricted` policy → `true`). The host runs the
    /// `bash` tool under a `bwrap --unshare-net` namespace. `false` = host network.
    pub deny_egress: bool,
}

/// One session-mounted resource (ADR-0038), parsed from a wire `resources[]` entry.
/// `kind` is the wire discriminant (`file` / `memory_store` / `github_repository`);
/// `id` is the backing reference (`file_id` / `memory_store_id` / repo `url`);
/// `mount_path` is where it appears in the sandbox; `instructions` is optional
/// per-binding guidance rendered into the system prompt.
#[derive(Debug, Clone)]
pub struct SessionResource {
    pub kind: String,
    pub id: String,
    pub mount_path: String,
    pub instructions: Option<String>,
    /// `github_repository` only: the GitHub PAT the host uses to clone/push. Never
    /// echoed back and never placed in the sandbox (host-side git transport only).
    pub auth_token: Option<String>,
    /// `github_repository` only: the branch to check out (`checkout.name`); `None`
    /// clones the remote's default branch.
    pub git_ref: Option<String>,
}

/// The repo name for a default mount path: the URL's last path segment, minus a
/// trailing `.git`. Falls back to `repo` when the URL has no usable segment.
fn repo_name(url: &str) -> String {
    let stem = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .strip_suffix(".git")
        .or_else(|| Some(url.trim_end_matches('/').rsplit('/').next().unwrap_or("")))
        .unwrap_or("");
    if stem.is_empty() {
        "repo".to_string()
    } else {
        stem.to_string()
    }
}

/// A wire `resources[]` entry — the official `BetaManagedAgents` resource union,
/// tagged by `type`. Unknown fields are ignored (tolerant of the full SDK payload);
/// an unknown `type` is a deserialize error (fail closed), never a silent drop.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireResource {
    File {
        file_id: String,
        #[serde(default)]
        mount_path: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
    },
    MemoryStore {
        memory_store_id: String,
        #[serde(default)]
        mount_path: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
    },
    GithubRepository {
        url: String,
        #[serde(default)]
        mount_path: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
        #[serde(default)]
        authorization_token: Option<String>,
        #[serde(default)]
        checkout: Option<WireCheckout>,
    },
}

/// A `github_repository` checkout selector. Only `branch` maps to a git ref today
/// (a `commit` sha clones the default branch, matching the prior behavior).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireCheckout {
    Branch {
        name: String,
    },
    // Accepted so the full SDK payload deserializes, but not yet wired to the clone
    // (the host checks out a branch ref; a `sha` clones the default branch). Parsed,
    // deliberately not consumed — see `into_session_resource`.
    Commit {
        #[allow(dead_code)]
        sha: String,
    },
}

impl WireResource {
    /// Lower to the neutral crate-boundary [`SessionResource`], defaulting the mount
    /// path per kind (mirroring the Managed defaults).
    fn into_session_resource(self) -> SessionResource {
        match self {
            WireResource::File {
                file_id,
                mount_path,
                instructions,
            } => SessionResource {
                kind: "file".into(),
                mount_path: mount_path.unwrap_or_else(|| format!("/mnt/session/uploads/{file_id}")),
                id: file_id,
                instructions,
                auth_token: None,
                git_ref: None,
            },
            WireResource::MemoryStore {
                memory_store_id,
                mount_path,
                instructions,
            } => SessionResource {
                kind: "memory_store".into(),
                mount_path: mount_path.unwrap_or_else(|| "/mnt/memory/store".into()),
                id: memory_store_id,
                instructions,
                auth_token: None,
                git_ref: None,
            },
            WireResource::GithubRepository {
                url,
                mount_path,
                instructions,
                authorization_token,
                checkout,
            } => SessionResource {
                // Repo default mirrors Managed Agents: /workspace/<repo-name>.
                mount_path: mount_path.unwrap_or_else(|| format!("/workspace/{}", repo_name(&url))),
                kind: "github_repository".into(),
                id: url,
                instructions,
                auth_token: authorization_token,
                git_ref: match checkout {
                    Some(WireCheckout::Branch { name }) => Some(name),
                    Some(WireCheckout::Commit { .. }) | None => None,
                },
            },
        }
    }
}

/// Parse one wire `resources[]` entry into a neutral [`SessionResource`]. `None` for
/// a malformed/unknown entry (the caller decides: session-create drops it; the live
/// `resources.add` path turns it into a 400). Shared by both paths.
fn parse_session_resource(v: &serde_json::Value) -> Option<SessionResource> {
    serde_json::from_value::<WireResource>(v.clone())
        .ok()
        .map(WireResource::into_session_resource)
}

/// Project a [`SessionResource`] to an official `BetaManagedAgentsSessionResource`
/// wire entry with a stable id (`{session}:resource:{n}`), so both create-time
/// backfill and live `resources.add` emit an SDK-decodable, uniformly-addressable
/// resource. The auth token is never echoed.
fn resource_dto(session_id: &str, n: usize, res: &SessionResource) -> serde_json::Value {
    use serde_json::json;
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(format!("{session_id}:resource:{n}")));
    obj.insert("type".into(), json!(res.kind));
    obj.insert("mount_path".into(), json!(res.mount_path));
    obj.insert("created_at".into(), json!(PROCESSED_AT));
    obj.insert("updated_at".into(), json!(PROCESSED_AT));
    match res.kind.as_str() {
        "file" => {
            obj.insert("file_id".into(), json!(res.id));
        }
        "memory_store" => {
            obj.insert("memory_store_id".into(), json!(res.id));
            if let Some(i) = &res.instructions {
                obj.insert("instructions".into(), json!(i));
            }
        }
        "github_repository" => {
            obj.insert("url".into(), json!(res.id));
            if let Some(r) = &res.git_ref {
                obj.insert("checkout".into(), json!({ "type": "branch", "name": r }));
            }
        }
        _ => {}
    }
    serde_json::Value::Object(obj)
}

/// One session MCP server, bound at creation: the wire name/url plus the vault
/// credential the URL matched (`None` when no vault credential matches — the
/// host then connects unauthenticated and the server decides). Consumed by
/// `ManagedHost::prepare_session` in the server assembly.
pub struct McpServerBinding {
    pub name: String,
    pub url: String,
    pub credential_source_id: Option<CredentialSourceId>,
    /// The matched credential's stored refresh configuration
    /// ([`VaultState::mcp_refresh_for_source`]), so the host can register a
    /// transport-level refresher next to the bearer. `None` when the credential
    /// is not refreshable (entered without a refresh object).
    pub refresh: Option<McpRefreshBinding>,
}

/// A queued live-inbox message on the session's in-flight turn. `id` is the
/// runtime's queue identity — targetable until the engine consumes the entry.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveInboxEntry {
    pub id: u64,
    pub content: Vec<ContentBlock>,
}

/// The session's live-inbox resource: the editable queue of messages addressed
/// to the in-flight turn. `active: false` means no native turn is running (the
/// queue shows empty; sends go through the normal event path instead).
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveInboxSnapshot {
    pub active: bool,
    pub version: u64,
    pub messages: Vec<LiveInboxEntry>,
}

impl LiveInboxSnapshot {
    pub fn inactive() -> Self {
        Self {
            active: false,
            version: 0,
            messages: Vec::new(),
        }
    }
}

/// Why a live-inbox operation failed. Mirrors the runtime contract's edit
/// errors, plus `Inactive` for "no native turn in flight on this session".
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LiveInboxError {
    #[error("no turn is in flight; send the message as a normal event")]
    Inactive,
    #[error("no queued message with that id")]
    UnknownMessage,
    #[error("proposed order does not match the current queue")]
    StaleOrder,
}

/// The runtime seam the adapter drives (DDD port). Implemented by the server over
/// the kernel; the adapter never constructs a runtime.
#[async_trait]
pub trait SessionRuntime: Send + Sync {
    /// Run one user turn on `thread` to its first pause or end. `content` is the
    /// user message's full block list (multimodal): text interleaved with any
    /// image blocks, never flattened to a bare string.
    async fn run_turn(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError>;

    /// Answer a built-in tool the run parked on (allow/deny) and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending built-in tool.
    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: Decision,
    ) -> Result<TurnOutcome, RunError>;

    /// Deliver a client-executed tool's result to the parked run and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending client-executed tool.
    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<TurnOutcome, RunError>;

    /// Provision `thread` for a new session BEFORE its record exists (ADR-0043
    /// Phase 3): the host materializes the init's MCP credential bindings and
    /// stages the servers for the thread's first turn. A failure fails the
    /// create (fail closed). The default is a no-op, so every host without MCP
    /// wiring is unaffected.
    async fn prepare_session(&self, _thread: &str, _init: SessionInit) -> Result<(), RunError> {
        Ok(())
    }

    /// Rebind `thread` to `model` for its subsequent turns (R5, per-turn override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next turn resolves the new executor.
    async fn rebind_model(&self, _thread: &str, _model: &str) -> Result<(), RunError> {
        Ok(())
    }

    /// Attach a resource to a LIVE session: stage its mount and make it take effect
    /// on the thread's next turn (the server impl merges it into the thread's staged
    /// resources and evicts the cached sandbox so the next turn rebuilds with it).
    /// The default is a no-op, so a host without resource staging is unaffected.
    async fn attach_resource(
        &self,
        _thread: &str,
        _resource: SessionResource,
    ) -> Result<(), RunError> {
        Ok(())
    }

    /// Detach a resource from a LIVE session: flush any write-back (memory) while the
    /// old sandbox is still live, drop this resource's mount, and evict the cached
    /// sandbox so the next turn rebuilds without it. Takes the resolved resource (not
    /// just an id) so the host has its mount path and kind. The default is a no-op.
    async fn detach_resource(
        &self,
        _thread: &str,
        _resource: SessionResource,
    ) -> Result<(), RunError> {
        Ok(())
    }

    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, _thread: &str) -> bool {
        false
    }

    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(&self, _thread: &str) -> Vec<Message> {
        Vec::new()
    }

    /// The session's accumulated token usage across all turns, surfaced on the
    /// session's `usage` field. The default is empty — a runtime that reports no usage
    /// (the deterministic in-process models).
    async fn session_usage(&self, _thread: &str) -> SessionUsage {
        SessionUsage::default()
    }

    /// Buffer a system message; it is prepended to the next turn's input.
    async fn add_system(&self, thread: &str, text: &str) -> Result<(), RunError>;

    /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
    /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
    async fn interrupt(&self, _thread: &str) -> Result<(), RunError> {
        Ok(())
    }

    /// Define an outcome and drive the grade->revise loop over `thread`, bounded by
    /// `max_iterations`; `rubric` is the normalized requirement text.
    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeReport, RunError>;

    /// The live-inbox queue on `thread`'s in-flight turn. The default reports
    /// an inactive queue, so a host without live-inbox wiring is unaffected.
    async fn live_inbox_snapshot(&self, _thread: &str) -> LiveInboxSnapshot {
        LiveInboxSnapshot::inactive()
    }

    /// Queue a message onto `thread`'s in-flight turn; it is folded into the
    /// running transcript at the next safe boundary. Fails `Inactive` when no
    /// native turn is running (the caller should send a normal event instead).
    async fn live_inbox_queue(
        &self,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// Delete one queued (not yet consumed) message.
    async fn live_inbox_remove(&self, _thread: &str, _id: u64) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// Replace one queued message's content, keeping its id and position.
    async fn live_inbox_replace(
        &self,
        _thread: &str,
        _id: u64,
        _content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// Reorder the queue to exactly `order` (a full permutation of current ids).
    async fn live_inbox_reorder(
        &self,
        _thread: &str,
        _order: Vec<u64>,
    ) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// The model id to echo in the session's agent object.
    fn model(&self) -> String;

    /// The advertised capability surface echoed in the session's agent object. The
    /// default reports nothing; a real host overrides it with its built-in tools,
    /// custom tools, skills, and delegate roster so the session enumerates what it does.
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::default()
    }
}

/// A runtime failure. `kind` classifies who is at fault so the router can map it
/// to the right HTTP status: a `BadRequest` is the caller's (an unknown park, a
/// mismatched id, a wrong-binding resume); `Internal` is the runtime's.
#[derive(Debug, thiserror::Error)]
#[error("run failed: {message}")]
pub struct RunError {
    pub message: String,
    pub kind: RunErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunErrorKind {
    Internal,
    BadRequest,
}

impl RunError {
    /// A runtime-side failure (provider error, corrupt state) — maps to `500`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Internal,
        }
    }

    /// A caller-side failure (bad id, wrong binding, no park) — maps to `400`.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::BadRequest,
        }
    }
}

struct SessionRecord {
    agent_id: String,
    session: Session,
    events: Vec<Event>,
    /// Subagent (multiagent delegate) child threads spawned in the session, each a
    /// projected `session_thread` object (parent = the primary thread). Enumerated
    /// by `list_threads`/`get_thread`; each is announced by a `session.thread_created`
    /// event (ADR-0047 D4, first slice).
    child_threads: Vec<serde_json::Value>,
}

/// The adapter's in-memory session store plus the runtime port.
pub struct ManagedState {
    runtime: Box<dyn SessionRuntime>,
    /// The vault surface, when the server mounts one (ADR-0043 Phase 3): a
    /// session's `mcp_servers` are bound to vault credentials through it at
    /// creation. `None` means every binding resolves to no credential.
    vaults: Option<Arc<VaultState>>,
    /// The environments surface, when the server mounts one: a session's
    /// `environment_id` is resolved to its networking policy (egress on/off) at
    /// creation. `None` → every session gets host network (unrestricted).
    environments: Option<Arc<crate::routes::environments::EnvironmentState>>,
    sessions: Mutex<HashMap<String, SessionRecord>>,
    /// The aspect-layer session→owner index (ADR-0051): the [`ScopeId`] that
    /// created each session, keyed by the tenancy-agnostic session id. It is NOT
    /// on the core session aggregate (which stays tenancy-agnostic) — it lives
    /// here so the edge ownership guard can 404 a cross-tenant request without the
    /// core ever reading a scope. Populated at `create_session` from the
    /// edge-resolved owner; read by [`ManagedState::owner_scope`].
    owners: Mutex<HashMap<String, String>>,
    /// Durable-config source of truth for the session aggregate: `create` writes
    /// it, rehydration reads it so a restored session reports its real
    /// agent/model/title/metadata/MCP instead of placeholder defaults. The
    /// in-memory `sessions` map is a per-process read-through cache over it.
    sessions_repo: Arc<dyn ManagedSessionRepository>,
    /// Optional projection sink for committed session lifecycle facts (ADR-0048):
    /// the assembly wires a webhook dispatcher here so a created/terminated session
    /// fans out to workspace-scoped subscriptions. `None` = no projection (the
    /// default, byte-identical to before). The wire crate stays webhook-agnostic —
    /// it only knows this narrow port.
    lifecycle_sink: Option<Arc<dyn SessionLifecycleSink>>,
    session_seq: AtomicU64,
    event_seq: AtomicU64,
}

/// A sink for committed session lifecycle facts, projected to external consumers
/// (webhooks). The Managed adapter calls it after a lifecycle transition commits,
/// handing the session's persisted owner (S3) so the consumer can stamp tenancy.
/// Implemented in the assembly layer over a webhook dispatcher; kept here so the
/// wire crate depends on no delivery machinery.
#[async_trait]
pub trait SessionLifecycleSink: Send + Sync {
    /// `event_type` is the `OutboundKind` wire name (e.g. `session.status_idle`);
    /// `workspace_id` is the session's owning workspace (absent on the bare
    /// pre-owner surface). The **org** is a deployment-level attribution the sink
    /// itself carries (from its assembly config / `AWAKEN_ORG_ID`), not a
    /// per-session axis — org is cloud-only (ADR-0048 D4), so the core never
    /// resolves it. Must not block the caller for long — deliver out-of-band.
    async fn emit(&self, session_id: &str, workspace_id: Option<&str>, event_type: &str);
}

/// The webhook lifecycle-fact catalog projected through [`SessionLifecycleSink`],
/// named to match Anthropic's official Managed Agents webhook event set. These are
/// a distinct vocabulary from the in-session SSE `OutboundKind` stream names: a
/// webhook consumer's `data.type` carries these past-tense *fact* names (the SSE
/// stream carries the present-tense transition names). Keeping them here — one
/// place, owned by the projecting crate — is why the wire crate needs no dependency
/// on the webhook delivery machinery: it emits fact names, the sink maps them.
///
/// Wired to the sink today: [`SESSION_IDLED`] (on create) and [`SESSION_TERMINATED`]
/// (on archive) — the two session-level transitions a webhook consumer acts on.
///
/// The rest of Anthropic's catalog is projected onto the SSE stream but not yet
/// fanned to webhooks, and maps to existing `OutboundKind` events: `session.status_
/// run_started` (`SessionStatusRunning`), `session.thread_created` (`SessionThread
/// Created`, whose webhook payload would carry `session_thread_id`), and `session.
/// outcome_evaluation_ended` (`SpanOutcomeEvaluationEnd`). Wiring one is additive —
/// add its const here and one `sink.emit` at the projection point — not a rename.
pub mod lifecycle_event {
    /// Session created, or a turn settled — now idle. Anthropic `session.status_idled`.
    pub const SESSION_IDLED: &str = "session.status_idled";
    /// Session terminated (archived). Anthropic `session.status_terminated`.
    pub const SESSION_TERMINATED: &str = "session.status_terminated";
}

/// Why a session operation failed (mapped to an HTTP status by the router).
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("session not found")]
    NotFound,
    /// A write was sent to an archived (terminated, read-only) session; the router
    /// maps it to 409 `invalid_request_error`.
    #[error("session is archived and is read-only")]
    Archived,
    /// A session create named a vault that does not exist (`vault_ids`); the
    /// router maps it to the standard 404 envelope naming the vault id.
    #[error("vault `{0}` not found")]
    VaultNotFound(String),
    #[error(transparent)]
    Run(#[from] RunError),
    /// A live-inbox operation was refused (inactive queue, unknown message,
    /// or a stale reorder); the router maps each case to its own status.
    #[error(transparent)]
    LiveInbox(#[from] LiveInboxError),
}

impl ManagedState {
    pub fn new(runtime: impl SessionRuntime + 'static) -> Self {
        Self {
            runtime: Box::new(runtime),
            vaults: None,
            environments: None,
            sessions: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            sessions_repo: Arc::new(InMemorySessionRepository::default()),
            lifecycle_sink: None,
            session_seq: AtomicU64::new(0),
            event_seq: AtomicU64::new(0),
        }
    }

    /// Wire a projection sink (a webhook dispatcher) so committed session lifecycle
    /// facts fan out to workspace-scoped subscribers (ADR-0048). Default: none.
    #[must_use]
    pub fn with_lifecycle_sink(mut self, sink: Arc<dyn SessionLifecycleSink>) -> Self {
        self.lifecycle_sink = Some(sink);
        self
    }

    /// Wire the environments surface, so `POST /v1/sessions` resolves the session's
    /// `environment_id` to its networking policy (egress on/off). Share the same
    /// `EnvironmentState` with [`crate::environments_router`], or the sessions and the
    /// environment routes see different environments.
    #[must_use]
    pub fn with_environments(
        mut self,
        environments: Arc<crate::routes::environments::EnvironmentState>,
    ) -> Self {
        self.environments = Some(environments);
        self
    }

    /// Wire a durable session repository (e.g. SQLite alongside the transcript
    /// store) so a session's config survives a restart and is reported faithfully
    /// by another process. The default is in-memory (single-process behavior).
    #[must_use]
    pub fn with_session_repo(mut self, repo: Arc<dyn ManagedSessionRepository>) -> Self {
        self.sessions_repo = repo;
        self
    }

    /// Wire the vault surface, so `POST /v1/sessions` binds each requested MCP
    /// server to a vault credential by URL (ADR-0043 Phase 3). Share the same
    /// `VaultState` with [`crate::vault_router`], or the sessions and the vault
    /// routes see different credentials.
    #[must_use]
    pub fn with_vaults(mut self, vaults: Arc<VaultState>) -> Self {
        self.vaults = Some(vaults);
        self
    }

    fn next_event_id(&self) -> String {
        format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
    }

    /// `POST /v1/sessions`.
    ///
    /// MCP binding (ADR-0043 Phase 3): each requested server is bound to a vault
    /// credential by exact `mcp_server_url` match across the request's
    /// `vault_ids`, then the runtime provisions the thread via
    /// [`SessionRuntime::prepare_session`] BEFORE the record is inserted — a
    /// failed preparation fails the create (fail closed; the router maps the
    /// `RunError` to the error envelope). A `vault_ids` entry that names no
    /// existing vault fails the create closed too ([`VaultState::has_vault`]):
    /// a 404 naming the vault id, BEFORE anything is provisioned — never a
    /// silent no-binding whose 401 only surfaces at the first turn. (Without a
    /// wired vault surface there is nothing to validate against and every
    /// binding resolves to no credential, as before.)
    /// Fail-closed bind-time legality check, shared by session creation and any
    /// pre-flight bind check: every vault a session references must exist. This is
    /// the one validation that must hold *before* an id is minted or a thread is
    /// prepared, so it lives in a single method rather than inline — a dry-run
    /// bind check calls exactly this, and gets exactly the error create would.
    pub fn check_bind(&self, req: &SessionCreateParams) -> Result<(), StateError> {
        if let Some(vaults) = &self.vaults
            && let Some(unknown) = req.vault_ids.iter().find(|v| !vaults.has_vault(v))
        {
            return Err(StateError::VaultNotFound(unknown.clone()));
        }
        Ok(())
    }

    pub async fn create_session(
        &self,
        req: SessionCreateParams,
        // The edge-resolved owning workspace (aspect): handed to the lifecycle
        // sink for webhook/usage stamping, but NEVER stored on the core session.
        workspace_id: Option<String>,
    ) -> Result<Session, StateError> {
        self.check_bind(&req)?;
        // Mint an id no durable thread already owns: a fresh process restarts
        // the sequence at 0, but the store dir may hold committed truth from a
        // previous process (ADR-0039). Adopting such a thread would graft the
        // old transcript onto a NEW session, so skip forward instead — the
        // rehydration path (`ensure_session`) remains the only way to reattach
        // to an existing thread, and it is keyed by the caller's explicit id.
        let id = loop {
            let candidate = format!("sesn_{}", self.session_seq.fetch_add(1, Ordering::SeqCst));
            if !self.runtime.owns_thread(&candidate).await {
                break candidate;
            }
        };
        let agent_id = req.agent.id().to_string();
        let bindings = req
            .mcp_servers
            .iter()
            .map(|server| {
                let credential_source_id = self
                    .vaults
                    .as_ref()
                    .and_then(|v| v.mcp_credential_source_for_url(&req.vault_ids, &server.url));
                // The matched credential's stored refresh configuration rides
                // along, so the host can keep the connection alive past the
                // access token's expiry.
                let refresh = match (&self.vaults, &credential_source_id) {
                    (Some(v), Some(source_id)) => v.mcp_refresh_for_source(source_id),
                    _ => None,
                };
                McpServerBinding {
                    name: server.name.clone(),
                    url: server.url.clone(),
                    credential_source_id,
                    refresh,
                }
            })
            .collect();
        // Parse the wire `resources[]` (ADR-0038) into staged mounts, and project each
        // into a DTO entry so the created session echoes its create-time resources —
        // list/get/delete then address these and any later-attached ones uniformly.
        let resources: Vec<SessionResource> = req
            .resources
            .iter()
            .filter_map(parse_session_resource)
            .collect();
        let resource_dtos: Vec<serde_json::Value> = resources
            .iter()
            .enumerate()
            .map(|(n, r)| resource_dto(&id, n, r))
            .collect();
        // Resolve the session's environment (defaulting to the local one) and its
        // networking policy once, for both the SessionInit (staged before the first
        // turn) and the echoed Session object.
        let environment_id = req
            .environment_id
            .clone()
            .unwrap_or_else(|| "env_local".to_string());
        let deny_egress = self
            .environments
            .as_ref()
            .is_some_and(|e| e.deny_egress(&environment_id));
        self.runtime
            .prepare_session(
                &id,
                SessionInit {
                    agent_id: agent_id.clone(),
                    mcp_servers: bindings,
                    resources,
                    model: req.awaken_model().map(str::to_string),
                    runtime: req.awaken_runtime().map(str::to_string),
                    deny_egress,
                },
            )
            .await
            .map_err(StateError::Run)?;
        // A session assigned to a self-hosted environment is dispatched through that
        // environment's work queue — the control plane enqueues it as `session` work
        // for an external worker to claim and run (the session still exists here; the
        // work item is how a polling worker discovers and drives it).
        if let Some(envs) = self.environments.as_ref() {
            if envs.is_self_hosted(&environment_id) {
                envs.enqueue_session_work(&environment_id, &id);
            }
        }
        // Enumerate the runtime's provisioned surface so the agent object reports what
        // the run can actually do (built-in toolset, custom tools, skills, delegates),
        // not an empty set. The wire shaping lives in `project`; the host supplies
        // neutral data.
        let caps = self.runtime.capabilities();
        let session = Session {
            id: id.clone(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: 1,
                // R6: echo the session's actual model — the requested override, else
                // the host default — so the client sees which model the session runs.
                model: ModelConfig::new(
                    req.awaken_model()
                        .map(str::to_string)
                        .unwrap_or_else(|| self.runtime.model()),
                ),
                name: agent_id.clone(),
                description: None,
                system: None,
                tools: project::agent_tools(&caps),
                // Echo the accepted servers in the SDK's `{name, type:"url", url}` shape.
                mcp_servers: req
                    .mcp_servers
                    .iter()
                    .map(|s| serde_json::to_value(s).expect("mcp server wire serializes"))
                    .collect(),
                skills: project::agent_skills(&caps),
                multiagent: project::agent_multiagent(&caps),
            },
            environment_id: environment_id.clone(),
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at: None,
            title: req.title,
            metadata: req.metadata,
            // The session's create-time mounts, echoed so the client can list/get them.
            resources: resource_dtos,
            outcome_evaluations: Vec::new(),
            status: "idle",
            stats: SessionStats::default(),
            usage: Usage::default(),
            vault_ids: req.vault_ids.clone(),
            deployment_id: None,
        };
        // Persist the session's config (secret-free) so a restart or a peer process
        // rehydrates its real agent/model/title/metadata/MCP, not a placeholder.
        // The core session record is tenancy-agnostic (authz is an edge aspect) —
        // it never stores a workspace/org.
        self.sessions_repo
            .save(PersistedSession {
                session_id: id.clone(),
                agent_id: agent_id.clone(),
                model: session.agent.model.id.clone(),
                title: session.title.clone(),
                metadata: session.metadata.clone(),
                environment_id: session.environment_id.clone(),
                mcp_servers: session.agent.mcp_servers.clone(),
            })
            .await;
        // Project the committed create as a lifecycle fact: a fresh session is idle,
        // so fan out `session.status_idled` (the webhook catalog name — past-tense
        // fact, distinct from the SSE `session.status_idle` transition) to any
        // workspace-scoped subscribers. The owning workspace comes from the edge (the
        // aspect), passed in — never read back from the core record. Out-of-band.
        if let Some(sink) = &self.lifecycle_sink {
            sink.emit(&id, workspace_id.as_deref(), lifecycle_event::SESSION_IDLED)
                .await;
        }
        // Record the session's owner (ADR-0051): in the aspect-layer in-memory
        // index (same-process) and — for a durable backend — beside the persisted
        // config, so the edge ownership guard fences a cross-tenant request even
        // across a restart that lost the index. A bare/self-hosted create (no
        // resolved workspace) owns under the seeded default scope.
        let owner_scope = workspace_id
            .clone()
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        self.owners
            .lock()
            .unwrap()
            .insert(id.clone(), owner_scope.clone());
        self.sessions_repo.set_owner(&id, &owner_scope).await;
        self.sessions.lock().unwrap().insert(
            id,
            SessionRecord {
                agent_id,
                session: session.clone(),
                events: Vec::new(),
                child_threads: Vec::new(),
            },
        );
        Ok(session)
    }

    /// The owner scope of `session_id`, if this process created (or has cached) it —
    /// the aspect-layer session→owner lookup the edge ownership guard consults
    /// (ADR-0051). `None` when the id is unknown to this process (e.g. a cross-
    /// process session before rehydration), where the guard falls through and the
    /// persistence layer remains the fence.
    #[must_use]
    pub fn owner_scope(&self, session_id: &str) -> Option<String> {
        self.owners.lock().unwrap().get(session_id).cloned()
    }

    /// Resolve the owner scope of `session_id` for the edge ownership guard,
    /// consulting the in-memory index first (same-process, no I/O) and then the
    /// durable store (cross-process, after a restart lost the index). `None` when
    /// no backend knows the session — a genuinely unknown id, where the guard
    /// falls through and the handler's own `NotFound` answers.
    pub async fn resolve_owner(&self, session_id: &str) -> Option<String> {
        if let Some(scope) = self.owner_scope(session_id) {
            return Some(scope);
        }
        self.sessions_repo.owner(session_id).await
    }

    /// A session object reconstructed for a rehydrated (post-restart) session.
    /// When the durable repo holds the session's config it is restored faithfully;
    /// otherwise (a session created before the repo existed, or a purely in-memory
    /// deployment) it falls back to the runtime's advertised surface with
    /// placeholder agent/title/metadata — the pre-repo behavior.
    fn rehydrated_session(&self, id: &str, persisted: Option<PersistedSession>) -> Session {
        let caps = self.runtime.capabilities();
        let (agent_id, model, environment_id, title, metadata, mcp_servers) = match persisted {
            Some(p) => (
                p.agent_id,
                p.model,
                p.environment_id,
                p.title,
                p.metadata,
                p.mcp_servers,
            ),
            None => (
                "assistant".to_string(),
                self.runtime.model(),
                "env_local".to_string(),
                None,
                Default::default(),
                Vec::new(),
            ),
        };
        Session {
            id: id.to_string(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: 1,
                model: ModelConfig::new(model),
                name: agent_id,
                description: None,
                system: None,
                tools: project::agent_tools(&caps),
                mcp_servers,
                skills: project::agent_skills(&caps),
                multiagent: project::agent_multiagent(&caps),
            },
            environment_id,
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at: None,
            title,
            metadata,
            // Non-durable: `PersistedSession` does not carry the create-time mounts,
            // and rehydration does not re-stage them into the host, so a rehydrated
            // session reports no resources. (Durable resources + restart re-staging
            // is a separate slice; storing the DTO alone would falsely show mounts
            // the sandbox no longer has.)
            resources: Vec::new(),
            outcome_evaluations: Vec::new(),
            status: "idle",
            stats: SessionStats::default(),
            usage: Usage::default(),
            vault_ids: Vec::new(),
            deployment_id: None,
        }
    }

    /// Recover a session whose in-memory record was lost from durable truth (a
    /// process restart, ADR-0039). If the store holds a committed transcript for
    /// `id`, rebuild the record — the projected history plus a reconstructed
    /// session object — so a resume can continue the parked run. A thread with no
    /// committed truth stays `NotFound` (fail closed): the store is authoritative.
    async fn ensure_session(&self, id: &str) -> Result<(), StateError> {
        if self.sessions.lock().unwrap().contains_key(id) {
            return Ok(());
        }
        let messages = self.runtime.committed_messages(id).await;
        if messages.is_empty() {
            return Err(StateError::NotFound);
        }
        let events: Vec<Event> = project_messages(&messages, None)
            .into_iter()
            .map(|event| Event {
                id: event.id.unwrap_or_else(|| self.next_event_id()),
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            })
            .collect();
        let persisted = self.sessions_repo.get(id).await;
        let agent_id = persisted
            .as_ref()
            .map_or_else(|| "assistant".to_string(), |p| p.agent_id.clone());
        let record = SessionRecord {
            agent_id,
            session: self.rehydrated_session(id, persisted),
            events,
            child_threads: Vec::new(),
        };
        self.sessions
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(record);
        Ok(())
    }

    /// `GET /v1/sessions/{id}`.
    pub fn get_session(&self, id: &str) -> Result<Session, StateError> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(id)
            .map(|r| r.session.clone())
            .ok_or(StateError::NotFound)
    }

    /// `GET /v1/sessions` — every session, ascending id (deterministic).
    pub fn list_sessions(&self) -> Vec<Session> {
        let sessions = self.sessions.lock().unwrap();
        let mut out: Vec<Session> = sessions.values().map(|r| r.session.clone()).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Sessions owned by `scope` — the tenancy-fenced list (ADR-0051), so a
    /// workspace's `GET /v1/sessions` never sees another's. A session with no
    /// recorded owner belongs to the seeded default scope. Mirrors the per-id
    /// ownership guard, which the collection route does not pass through.
    pub fn list_sessions_scoped(&self, scope: &str) -> Vec<Session> {
        // Snapshot owners first (lock, clone, drop) so we never hold two locks at
        // once — create_session takes `owners` on its own path.
        let owners = self.owners.lock().unwrap().clone();
        let sessions = self.sessions.lock().unwrap();
        let mut out: Vec<Session> = sessions
            .values()
            .filter(|r| {
                owners
                    .get(&r.session.id)
                    .map_or(scope == DEFAULT_SCOPE, |owner| owner == scope)
            })
            .map(|r| r.session.clone())
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// `POST /v1/sessions/{id}` — update `title` and/or PATCH `metadata`
    /// (string upserts, null deletes, omitted preserves).
    /// `POST /v1/sessions/{id}` — update only `title` / `metadata`. `environment_id`
    /// is pinned at session creation and is not accepted here (Managed Agents
    /// contract: the container's environment is fixed for the session's lifetime —
    /// to change it, create a new session), so a caller sending it is ignored.
    pub fn update_session(
        &self,
        id: &str,
        title: Option<Option<String>>,
        metadata: Option<std::collections::BTreeMap<String, Option<String>>>,
    ) -> Result<Session, StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        let title_in_request = title.is_some();
        if let Some(title) = title {
            record.session.title = title;
        }
        if let Some(patch) = metadata {
            for (key, value) in patch {
                match value {
                    Some(v) => {
                        record.session.metadata.insert(key, v);
                    }
                    None => {
                        record.session.metadata.remove(&key);
                    }
                }
            }
        }
        // Announce the mutation on the event stream (`session.updated`): the new
        // title when the update set one, plus the full metadata bag.
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionUpdated {
                title: title_in_request
                    .then(|| record.session.title.clone())
                    .flatten(),
                metadata: record.session.metadata.clone(),
            },
            processed_at: Some(PROCESSED_AT.to_string()),
        });
        Ok(record.session.clone())
    }

    /// `DELETE /v1/sessions/{id}` — drop the in-memory record.
    pub fn delete_session(&self, id: &str) -> Result<(), StateError> {
        self.sessions
            .lock()
            .unwrap()
            .remove(id)
            .map(|_| ())
            .ok_or(StateError::NotFound)
    }

    /// `POST /v1/sessions/{id}/archive` — terminate the session: stamp
    /// `archived_at`, move `status` to `terminated`, and commit a
    /// `session.status_terminated` event so a streaming/listing client observes the
    /// terminal transition (not just the mutated status field). Idempotent: a
    /// re-archive returns the same terminal record without a second event.
    pub async fn archive_session(&self, id: &str) -> Result<Session, StateError> {
        // Mutate under the lock, then release it before any await (the sink is async,
        // and a std `MutexGuard` must not be held across `.await`). `newly_terminated`
        // gates the projection so a re-archive (idempotent) fans out no second event.
        let (session, newly_terminated) = {
            let terminated_id = self.next_event_id();
            let mut sessions = self.sessions.lock().unwrap();
            let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
            let newly = record.session.archived_at.is_none();
            if newly {
                record.session.archived_at = Some(PROCESSED_AT.to_string());
                record.session.status = "terminated";
                record.events.push(Event {
                    id: terminated_id,
                    kind: OutboundKind::SessionStatusTerminated {},
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            (record.session.clone(), newly)
        };
        // Project the terminal transition as a lifecycle fact, mirroring create's
        // `session.status_idled`. The owning workspace is resolved from the session's
        // persisted owner (the archive edge carries only the id) so a subscription in
        // that workspace is matched even after a restart lost the in-memory index.
        if newly_terminated {
            if let Some(sink) = &self.lifecycle_sink {
                let owner = self.resolve_owner(id).await;
                sink.emit(id, owner.as_deref(), lifecycle_event::SESSION_TERMINATED)
                    .await;
            }
        }
        Ok(session)
    }

    /// The session's primary thread projection (`BetaManagedAgentsSessionThread`).
    /// A session has one primary thread addressed by `<session_id>:primary`;
    /// sub-threads spawned by a multiagent turn would extend this list.
    fn primary_thread(record: &SessionRecord) -> serde_json::Value {
        let session = &record.session;
        serde_json::json!({
            "id": format!("{}:primary", session.id),
            "type": "session_thread",
            "session_id": session.id,
            "parent_thread_id": null,
            "agent": session.agent,
            "created_at": session.created_at,
            "updated_at": session.updated_at,
            "archived_at": session.archived_at,
            "status": session.status,
            "stats": null,
            "usage": null,
        })
    }

    /// A subagent child thread: a `session_thread` whose parent is the primary and
    /// whose `agent` is a minimal snapshot of the delegate `agent_name`.
    fn child_thread(session: &Session, thread_id: &str, agent_name: &str) -> serde_json::Value {
        // Reuse the one `SessionAgent` shape rather than rebuild the agent object
        // inline; the delegate's config is unknown here, so it is a minimal snapshot.
        let agent = SessionAgent {
            id: agent_name.to_string(),
            kind: "agent",
            version: 1,
            model: ModelConfig::new(""),
            name: agent_name.to_string(),
            description: None,
            system: None,
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            multiagent: None,
        };
        serde_json::json!({
            "id": thread_id,
            "type": "session_thread",
            "session_id": session.id,
            "parent_thread_id": format!("{}:primary", session.id),
            "agent": agent,
            "created_at": session.created_at,
            "updated_at": session.updated_at,
            "archived_at": null,
            "status": session.status,
            "stats": null,
            "usage": null,
        })
    }

    /// `GET /v1/sessions/{id}/threads` — the primary thread plus any subagent
    /// child threads spawned by delegation.
    pub fn list_threads(&self, id: &str) -> Result<Vec<serde_json::Value>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        let mut threads = vec![Self::primary_thread(record)];
        threads.extend(record.child_threads.iter().cloned());
        Ok(threads)
    }

    /// `GET /v1/sessions/{id}/threads/{thread_id}` — the primary or a child thread.
    pub fn get_thread(&self, id: &str, thread_id: &str) -> Result<serde_json::Value, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        let primary = Self::primary_thread(record);
        if primary["id"] == thread_id {
            return Ok(primary);
        }
        record
            .child_threads
            .iter()
            .find(|t| t["id"] == thread_id)
            .cloned()
            .ok_or(StateError::NotFound)
    }

    /// `POST /v1/sessions/{id}/threads/{thread_id}/archive`. Archiving the primary
    /// thread archives the session; archiving a subagent child thread terminates
    /// that thread (emitting `session.thread_status_terminated`).
    pub fn archive_thread(
        &self,
        id: &str,
        thread_id: &str,
    ) -> Result<serde_json::Value, StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        if format!("{id}:primary") == thread_id {
            record.session.archived_at = Some(PROCESSED_AT.to_string());
            return Ok(Self::primary_thread(record));
        }
        let Some(child) = record
            .child_threads
            .iter_mut()
            .find(|t| t["id"] == thread_id)
        else {
            return Err(StateError::NotFound);
        };
        child["archived_at"] = serde_json::json!(PROCESSED_AT);
        child["status"] = serde_json::json!("terminated");
        let agent_name = child["agent"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let archived = child.clone();
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionThreadStatusTerminated {
                session_thread_id: thread_id.to_string(),
                agent_name,
            },
            processed_at: Some(PROCESSED_AT.to_string()),
        });
        Ok(archived)
    }

    /// `GET /v1/sessions/{id}/resources` — the session's mounted resources.
    pub fn list_resources(&self, id: &str) -> Result<Vec<serde_json::Value>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        Ok(record.session.resources.clone())
    }

    /// `POST /v1/sessions/{id}/resources` — mount a resource on a live session,
    /// minting an id. `file` and `github_repository` are attachable here; a
    /// `memory_store` is bound at session creation only (Managed Agents contract),
    /// so adding one to a running session fails closed with a 400.
    ///
    /// The mount is realized: the runtime stages it and evicts the thread's cached
    /// sandbox so the NEXT turn rebuilds with the resource present — not merely a
    /// record edit. The session record then echoes the resource for list/get/delete.
    pub async fn create_resource(
        &self,
        id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, StateError> {
        // The session must exist (checked without holding the lock across the await).
        {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(id).ok_or(StateError::NotFound)?;
        }
        if body.get("type").and_then(|t| t.as_str()) == Some("memory_store") {
            return Err(StateError::Run(RunError::bad_request(MEMORY_CREATE_ONLY)));
        }
        let res = parse_session_resource(&body).ok_or_else(|| {
            StateError::Run(RunError::bad_request(
                "resource must be a file or github_repository with its backing id",
            ))
        })?;
        // Make it real before recording it: stage into the host + evict the cached
        // sandbox. A staging failure fails the request (fail closed, no record edit).
        self.runtime
            .attach_resource(id, res.clone())
            .await
            .map_err(StateError::Run)?;
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        let n = record.session.resources.len();
        let dto = resource_dto(id, n, &res);
        record.session.resources.push(dto.clone());
        Ok(dto)
    }

    /// `GET /v1/sessions/{id}/resources/{resource_id}`.
    pub fn get_resource(
        &self,
        id: &str,
        resource_id: &str,
    ) -> Result<serde_json::Value, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        record
            .session
            .resources
            .iter()
            .find(|r| r["id"] == resource_id)
            .cloned()
            .ok_or(StateError::NotFound)
    }

    /// `POST /v1/sessions/{id}/resources/{resource_id}` — merge a JSON patch.
    pub fn update_resource(
        &self,
        id: &str,
        resource_id: &str,
        patch: serde_json::Value,
    ) -> Result<serde_json::Value, StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        let resource = record
            .session
            .resources
            .iter_mut()
            .find(|r| r["id"] == resource_id)
            .ok_or(StateError::NotFound)?;
        if let (Some(target), Some(patch)) = (resource.as_object_mut(), patch.as_object()) {
            for (k, v) in patch {
                target.insert(k.clone(), v.clone());
            }
        }
        Ok(resource.clone())
    }

    /// `DELETE /v1/sessions/{id}/resources/{resource_id}` — detach a `file` or
    /// `github_repository` from a live session. A `memory_store` binds at session
    /// creation and cannot be removed from a running session (Managed Agents
    /// contract), so detaching one fails closed with a 400.
    pub async fn delete_resource(&self, id: &str, resource_id: &str) -> Result<(), StateError> {
        // Resolve the target (existence + kind) under the lock, dropped before the
        // await. `memory_store` cannot be detached from a running session.
        let res = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(id).ok_or(StateError::NotFound)?;
            let target = record
                .session
                .resources
                .iter()
                .find(|r| r["id"] == resource_id)
                .ok_or(StateError::NotFound)?;
            if target.get("type").and_then(|t| t.as_str()) == Some("memory_store") {
                return Err(StateError::Run(RunError::bad_request(MEMORY_CREATE_ONLY)));
            }
            parse_session_resource(target)
        };
        // The runtime flushes write-back while the old sandbox is still live, drops
        // this resource's mount, and evicts the cached sandbox so the next turn
        // rebuilds without it. Then the record drops the entry.
        if let Some(res) = res {
            self.runtime
                .detach_resource(id, res)
                .await
                .map_err(StateError::Run)?;
        }
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        record.session.resources.retain(|r| r["id"] != resource_id);
        Ok(())
    }

    /// Append one step's projected events to the session, minting ids where the
    /// projection did not supply one.
    fn append_turn(&self, session_id: &str, outcome: TurnOutcome) -> Result<(), StateError> {
        let pending = outcome
            .pending
            .as_ref()
            .map(|p| (p.tool_use_id.as_str(), p.client_executed));
        let projected = project_turn(&outcome.messages, outcome.stop, pending);
        // Delegation runs inline as an `agent_run` tool call; each one spawns a
        // subagent child thread (ADR-0047 D4). Collect each delegate's name, the
        // input it was sent, and the reply it returned (matched by tool-use id),
        // before the projected events are consumed.
        struct DelegateCall {
            agent_name: String,
            tool_use_id: Option<String>,
            sent: Vec<ContentBlock>,
            received: Vec<ContentBlock>,
        }
        let mut delegates: Vec<DelegateCall> = projected
            .iter()
            .filter_map(|e| match &e.kind {
                OutboundKind::AgentToolUse { name, input, .. } if name == "agent_run" => {
                    Some(DelegateCall {
                        agent_name: input
                            .get("agent_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        tool_use_id: e.id.clone(),
                        sent: vec![ContentBlock::text(
                            input
                                .get("input")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default(),
                        )],
                        received: Vec::new(),
                    })
                }
                _ => None,
            })
            .collect();
        for e in &projected {
            if let OutboundKind::AgentToolResult {
                tool_use_id,
                content,
                ..
            } = &e.kind
            {
                if let Some(d) = delegates
                    .iter_mut()
                    .find(|d| d.tool_use_id.as_deref() == Some(tool_use_id.as_str()))
                {
                    d.received = content.clone();
                }
            }
        }
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        // Each processing segment is bracketed `running` … `idle`; the running
        // marker leads before any fold or message.
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionStatusRunning {},
            processed_at: Some(PROCESSED_AT.to_string()),
        });
        // Compaction ran at BeforeInference, so its marker precedes the turn's
        // message events. `true` ⇒ this terminal step folded (emit-once upstream).
        if outcome.compacted {
            record.events.push(Event {
                id: self.next_event_id(),
                kind: OutboundKind::ThreadContextCompacted {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // A terminal run fault projects a `session.error` before the turn's idle,
        // so a streaming/listing client observes the failure. The neutral fault
        // `message` is carried through; the SDK's `unknown_error` fallback is the
        // honest projection until the runtime classifies faults per-variant.
        if let Some(failure) = &outcome.failure {
            record.events.push(Event {
                id: self.next_event_id(),
                kind: OutboundKind::SessionError {
                    error: SessionError::exhausted(failure.message.clone()),
                },
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        for event in projected {
            let id = event.id.unwrap_or_else(|| self.next_event_id());
            record.events.push(Event {
                id,
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // Register a child thread per delegate call and project its full inline
        // lifecycle: `created` → `status_running` → the input sent to the delegate →
        // the reply received → `status_idle`. The thread API enumerates subagent
        // threads and the stream carries their messages and status.
        for d in delegates {
            let thread_id = format!("{}:thread:{}", session_id, record.child_threads.len());
            record.child_threads.push(Self::child_thread(
                &record.session,
                &thread_id,
                &d.agent_name,
            ));
            let name = d.agent_name;
            for kind in [
                OutboundKind::SessionThreadCreated {
                    session_thread_id: thread_id.clone(),
                    agent_name: name.clone(),
                },
                OutboundKind::SessionThreadStatusRunning {
                    session_thread_id: thread_id.clone(),
                    agent_name: name.clone(),
                },
                OutboundKind::AgentThreadMessageSent {
                    to_session_thread_id: thread_id.clone(),
                    to_agent_name: name.clone(),
                    content: d.sent,
                },
                OutboundKind::AgentThreadMessageReceived {
                    from_session_thread_id: thread_id.clone(),
                    from_agent_name: name.clone(),
                    content: d.received,
                },
                OutboundKind::SessionThreadStatusIdle {
                    session_thread_id: thread_id.clone(),
                    agent_name: name.clone(),
                    stop_reason: StopReason::EndTurn,
                },
            ] {
                record.events.push(Event {
                    id: self.next_event_id(),
                    kind,
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
        }
        Ok(())
    }

    /// Append an outcome report: for each round, the agent's revision events then
    /// `span.outcome_evaluation_start` / `_end`, and a terminal `session.status_idle`.
    fn append_outcome(&self, session_id: &str, report: OutcomeReport) -> Result<(), StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        // Each round's durable evaluation record, collected as we project its events
        // and folded into the session object after the event-pushing borrow releases.
        let mut evaluations: Vec<serde_json::Value> = Vec::new();
        {
            let mut push = |id: Option<String>, kind: OutboundKind| {
                record.events.push(Event {
                    id: id.unwrap_or_else(|| {
                        format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
                    }),
                    kind,
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            };
            push(None, OutboundKind::SessionStatusRunning {});
            for round in report.iterations {
                for event in project_messages(&round.messages, None) {
                    push(event.id, event.kind);
                }
                evaluations.push(project::outcome_evaluation(&round));
                push(
                    None,
                    OutboundKind::SpanOutcomeEvaluationStart {
                        outcome_id: round.outcome_id.clone(),
                        iteration: round.iteration,
                    },
                );
                push(
                    None,
                    OutboundKind::SpanOutcomeEvaluationOngoing {
                        outcome_id: round.outcome_id.clone(),
                        iteration: round.iteration,
                    },
                );
                push(
                    None,
                    OutboundKind::SpanOutcomeEvaluationEnd {
                        outcome_id: round.outcome_id,
                        iteration: round.iteration,
                        result: round.result,
                        explanation: round.explanation,
                    },
                );
            }
            push(
                None,
                OutboundKind::SessionStatusIdle {
                    stop_reason: StopReason::EndTurn,
                },
            );
        }
        // The session object carries the running list of evaluations that have graded
        // it, so a `GET /v1/sessions/{id}` reflects the outcomes that ran, not [].
        record.session.outcome_evaluations.extend(evaluations);
        Ok(())
    }

    /// `POST /v1/sessions/{id}/events`. Mints a receipt per inbound event and acts
    /// on `user.message` (run a turn) and `user.tool_confirmation` (resume a
    /// parked run), appending the projected events.
    /// Resolve `session_id` (rehydrating from durable truth after a restart,
    /// like `send_events`) and fail closed when it names no session.
    async fn require_session(&self, session_id: &str) -> Result<(), StateError> {
        self.ensure_session(session_id).await?;
        let sessions = self.sessions.lock().unwrap();
        if sessions.contains_key(session_id) {
            Ok(())
        } else {
            Err(StateError::NotFound)
        }
    }

    /// `GET /v1/sessions/:id/live-inbox` — the in-flight turn's editable queue.
    pub async fn live_inbox_snapshot(
        &self,
        session_id: &str,
    ) -> Result<LiveInboxSnapshot, StateError> {
        self.require_session(session_id).await?;
        Ok(self.runtime.live_inbox_snapshot(session_id).await)
    }

    /// `POST /v1/sessions/:id/live-inbox` — queue a message for the in-flight turn.
    pub async fn live_inbox_queue(
        &self,
        session_id: &str,
        content: Vec<ContentBlock>,
    ) -> Result<u64, StateError> {
        self.require_session(session_id).await?;
        Ok(self.runtime.live_inbox_queue(session_id, content).await?)
    }

    /// `DELETE /v1/sessions/:id/live-inbox/:msg` — withdraw a queued message.
    pub async fn live_inbox_remove(&self, session_id: &str, id: u64) -> Result<(), StateError> {
        self.require_session(session_id).await?;
        Ok(self.runtime.live_inbox_remove(session_id, id).await?)
    }

    /// `PUT /v1/sessions/:id/live-inbox/:msg` — replace a queued message's content.
    pub async fn live_inbox_replace(
        &self,
        session_id: &str,
        id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), StateError> {
        self.require_session(session_id).await?;
        Ok(self
            .runtime
            .live_inbox_replace(session_id, id, content)
            .await?)
    }

    /// `PUT /v1/sessions/:id/live-inbox/order` — reorder the queue (full permutation).
    pub async fn live_inbox_reorder(
        &self,
        session_id: &str,
        order: Vec<u64>,
    ) -> Result<(), StateError> {
        self.require_session(session_id).await?;
        Ok(self.runtime.live_inbox_reorder(session_id, order).await?)
    }

    #[tracing::instrument(
        name = "sessions.events.send",
        skip_all,
        fields(gen_ai.conversation.id = %session_id)
    )]
    pub async fn send_events(
        &self,
        session_id: &str,
        req: SendEventsRequest,
    ) -> Result<SendEventsResponse, StateError> {
        // Recover the session from durable truth if its in-memory record was lost
        // (a process restart) before resolving the agent — so a resume continues
        // the parked run instead of failing closed (ADR-0039).
        self.ensure_session(session_id).await?;
        // An archived session is terminal and read-only: refuse every inbound write
        // (message, resume, interrupt, outcome) with a 409, before touching the
        // runtime — the contract makes an archived session read-only.
        let agent_id = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
            if record.session.archived_at.is_some() {
                return Err(StateError::Archived);
            }
            record.agent_id.clone()
        };

        let mut receipts = Vec::new();
        for inbound in &req.events {
            receipts.push(EventReceipt {
                id: self.next_event_id(),
                kind: inbound.type_str(),
                processed_at: None,
            });

            match inbound {
                InboundEvent::UserMessage { content, model, .. } => {
                    // R5: a per-turn model override rebinds the thread before the turn.
                    if let Some(model) = model {
                        self.runtime.rebind_model(session_id, model).await?;
                    }
                    let outcome = self
                        .runtime
                        .run_turn(&agent_id, session_id, content.clone())
                        .await?;
                    self.append_turn(session_id, outcome)?;
                }
                InboundEvent::UserToolConfirmation {
                    tool_use_id,
                    result,
                    deny_message,
                } => {
                    let decision = Decision {
                        allow: matches!(result, ConfirmResult::Allow),
                        note: deny_message.clone(),
                    };
                    let outcome = self
                        .runtime
                        .resume(session_id, tool_use_id, decision)
                        .await?;
                    self.append_turn(session_id, outcome)?;
                }
                InboundEvent::UserCustomToolResult {
                    custom_tool_use_id,
                    content,
                    is_error,
                } => {
                    let text = content.as_deref().map(content_text).unwrap_or_default();
                    let outcome = self
                        .runtime
                        .resume_custom(session_id, custom_tool_use_id, &text, *is_error)
                        .await?;
                    self.append_turn(session_id, outcome)?;
                }
                // The generic `user.tool_result`: a client-provided result for a
                // parked tool, keyed by `tool_use_id`. Same delivery as a custom
                // tool result (the id addresses the parked tool either way).
                InboundEvent::UserToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let text = content.as_deref().map(content_text).unwrap_or_default();
                    let outcome = self
                        .runtime
                        .resume_custom(session_id, tool_use_id, &text, *is_error)
                        .await?;
                    self.append_turn(session_id, outcome)?;
                }
                InboundEvent::UserDefineOutcome {
                    description,
                    rubric,
                    max_iterations,
                } => {
                    let rubric = rubric_text(rubric);
                    let report = self
                        .runtime
                        .define_outcome(
                            session_id,
                            description,
                            &rubric,
                            max_iterations.unwrap_or(3),
                        )
                        .await?;
                    self.append_outcome(session_id, report)?;
                }
                InboundEvent::SystemMessage { content } => {
                    let text = content_text(content);
                    self.runtime.add_system(session_id, &text).await?;
                }
                // `user.interrupt`: cancel the run in flight on this thread (from a
                // concurrent request), so an in-progress outcome ends `interrupted`.
                InboundEvent::UserInterrupt { .. } => {
                    self.runtime.interrupt(session_id).await?;
                }
            }
        }
        // Refresh the session's accumulated token usage from the runtime's committed
        // tally, so a subsequent GET /v1/sessions reflects the tokens this turn spent.
        let usage = self.runtime.session_usage(session_id).await;
        if let Some(record) = self.sessions.lock().unwrap().get_mut(session_id) {
            record.session.usage = session_usage_value(usage);
        }
        Ok(SendEventsResponse { data: receipts })
    }

    /// `GET /v1/sessions/{id}/events`.
    pub fn list_events(&self, session_id: &str) -> Result<ListEventsResponse, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok(ListEventsResponse {
            data: record.events.clone(),
            next_page: None,
            has_more: false,
        })
    }

    /// `GET /v1/sessions/{id}/events/stream` — the events to replay as SSE.
    pub fn stream_events(&self, session_id: &str) -> Result<Vec<Event>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok(record.events.clone())
    }
}

/// Normalize a Managed rubric (a bare string or `{type:"text",content}`) to text.
fn rubric_text(rubric: &serde_json::Value) -> String {
    match rubric {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// The session-level token usage the managed wire reports (the port's neutral shape;
/// the runtime's per-model `TokenUsage` totals are mapped onto this by the host, so
/// this crate needs no runtime-plane type). Cumulative across all turns and models.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

/// The session's `usage` object (`BetaManagedAgentsSessionUsage`): cumulative input +
/// output (+ prompt-cache) token counts across all turns. Emitted whenever a turn ran.
fn session_usage_value(usage: SessionUsage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_tokens,
        cache_creation_input_tokens: usage.cache_creation_tokens,
    }
}

/// Concatenate the text of a content-block list.
fn content_text(content: &[awaken_agent_contract::agent::content::ContentBlock]) -> String {
    use awaken_agent_contract::agent::content::ContentBlock;
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A runtime that reports a non-empty committed transcript, so a session can
    /// rehydrate. Every operational method is unused by these tests.
    struct RehydrateFake;

    #[async_trait]
    impl SessionRuntime for RehydrateFake {
        async fn run_turn(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<TurnOutcome, RunError> {
            unreachable!()
        }
        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: Decision,
        ) -> Result<TurnOutcome, RunError> {
            unreachable!()
        }
        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: &str,
            _is_error: bool,
        ) -> Result<TurnOutcome, RunError> {
            unreachable!()
        }
        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            Ok(())
        }
        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeReport, RunError> {
            unreachable!()
        }
        async fn committed_messages(&self, thread: &str) -> Vec<Message> {
            vec![Message::text(
                awaken_agent_contract::agent::message::Id(format!("{thread}-m0")),
                awaken_agent_contract::agent::message::Role::User,
                "hello",
            )]
        }
        fn model(&self) -> String {
            "host-default-model".to_string()
        }
    }

    fn sample_persisted(id: &str) -> PersistedSession {
        let mut metadata = BTreeMap::new();
        metadata.insert("team".to_string(), "research".to_string());
        PersistedSession {
            session_id: id.to_string(),
            agent_id: "coder".to_string(),
            model: "kimi-k2".to_string(),
            title: Some("My session".to_string()),
            metadata,
            environment_id: "env_local".to_string(),
            mcp_servers: vec![
                serde_json::json!({"name": "calc", "type": "url", "url": "https://x"}),
            ],
        }
    }

    #[test]
    fn rehydrated_session_restores_persisted_config() {
        let state = ManagedState::new(RehydrateFake);
        let session = state.rehydrated_session("sesn_1", Some(sample_persisted("sesn_1")));
        assert_eq!(session.agent.id, "coder");
        assert_eq!(session.agent.model.id, "kimi-k2");
        assert_eq!(session.title.as_deref(), Some("My session"));
        assert_eq!(
            session.metadata.get("team").map(String::as_str),
            Some("research")
        );
        assert_eq!(
            session.agent.mcp_servers.len(),
            1,
            "the accepted MCP server is restored"
        );
    }

    #[test]
    fn rehydrated_session_falls_back_without_persisted_config() {
        let state = ManagedState::new(RehydrateFake);
        let session = state.rehydrated_session("sesn_1", None);
        assert_eq!(session.agent.id, "assistant");
        assert_eq!(session.agent.model.id, "host-default-model");
        assert!(session.title.is_none());
        assert!(session.agent.mcp_servers.is_empty());
    }

    #[tokio::test]
    async fn ensure_session_rehydrates_from_repo_after_cache_loss() {
        // A session created in one process is gone from a fresh process's cache,
        // but the shared repo + committed transcript restore it faithfully.
        let repo: Arc<dyn ManagedSessionRepository> =
            Arc::new(InMemorySessionRepository::default());
        repo.save(sample_persisted("sesn_1")).await;

        // Fresh state (empty cache) sharing the durable repo — simulates a restart.
        let restarted = ManagedState::new(RehydrateFake).with_session_repo(repo);
        restarted.ensure_session("sesn_1").await.expect("rehydrate");
        let session = restarted
            .get_session("sesn_1")
            .expect("session present after rehydrate");
        assert_eq!(
            session.agent.id, "coder",
            "real agent id, not the placeholder"
        );
        assert_eq!(session.title.as_deref(), Some("My session"));
        assert_eq!(session.agent.mcp_servers.len(), 1);
    }
}
