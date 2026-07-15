//! The management ("admin") assistant's tools — ADR-0052.
//!
//! This is the platform's FAB-equivalent: an ordinary agent (authored, compiled,
//! published like any other) whose *privilege* is expressed entirely by the scope it
//! lives in and the tools visible in that scope (ADR-0052 D1–D3), not by a special
//! agent type or a field on any value object. This crate owns only the management
//! tools and their descriptors, plus the seeded system prompt; the scope fence,
//! seeding, model auto-binding, and audit live in the host (D2/D3/D5/D6).
//!
//! Four tools author agent configurations: they read the org-shared capability view,
//! build/refine a FULL [`AgentConfig`] from a flattened intent, validate it, and
//! persist it as an **unpublished** draft (exactly what the editor's Save does). There
//! is deliberately **no publish tool**: publication is a console action, never an LLM
//! tool call (D4). A fifth, read-only tool ([`EXPLAIN_TOOL`]) turns the assistant into
//! an in-console user manual (answers how-to/what-is questions from a curated corpus).
//!
//! The tools reach real platform state through three ports the host implements
//! ([`CapabilityReader`], [`DraftValidator`], [`DraftStore`]) — so this crate stays a
//! leaf that names only the neutral tool contract and the config aggregate, never the
//! server or the config service/plane.

#![forbid(unsafe_code)]

mod console_help;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_config_store::{AgentConfig, ModelSelection, ToolOverride};
use awaken_runtime_contract::resolved::{ContextPolicy, ToolDescriptor};
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

/// The four management tool ids. They are namespaced `admin_*` and are only ever
/// nameable in the reserved scope (the fence is the scope-keyed catalog projection,
/// ADR-0052 D3 — enforced in the host, not here).
pub const CAPABILITIES_TOOL: &str = "admin_get_platform_capabilities";
/// Draft (author + persist unpublished) a full agent config from a flattened intent.
pub const CREATE_DRAFT_TOOL: &str = "admin_draft_agent";
/// Incrementally refine a persisted draft with a small patch.
pub const PATCH_TOOL: &str = "admin_patch_agent";
/// Validate a persisted draft by id.
pub const VALIDATE_TOOL: &str = "admin_validate_agent";
/// Explain how to use the console (read-only help): return a curated topic, or the
/// topic index. Lets the assistant double as an in-console user manual.
pub const EXPLAIN_TOOL: &str = "admin_explain_console";

/// The reserved agent id the management assistant is published under. It is a
/// deliberately un-tenant-like id, seeded once into the reserved scope (ADR-0052 D2).
pub const ADMIN_ASSISTANT_AGENT_ID: &str = "__admin_assistant";

/// All management tool ids, in advertised order.
#[must_use]
pub fn admin_tool_ids() -> Vec<String> {
    vec![
        CAPABILITIES_TOOL.to_string(),
        CREATE_DRAFT_TOOL.to_string(),
        PATCH_TOOL.to_string(),
        VALIDATE_TOOL.to_string(),
        EXPLAIN_TOOL.to_string(),
    ]
}

/// The seed [`AgentConfig`] for the management assistant (ADR-0052 D1/D3/D4): an
/// **ordinary** config — instructions + the four admin tool ids + an `Auto` model
/// binding — with the native backend (no sandbox). The host publishes it into the
/// reserved scope through the ordinary publish path, so it becomes a compiled,
/// content-addressed `RunnableConfig` like any agent (no builder bypass).
#[must_use]
pub fn admin_assistant_config() -> AgentConfig {
    AgentConfig {
        id: ADMIN_ASSISTANT_AGENT_ID.to_string(),
        instructions: ADMIN_ASSISTANT_INSTRUCTIONS.to_string(),
        max_steps: 12,
        model_binding: ModelSelection::Auto,
        tool_ids: admin_tool_ids(),
        plugin_ids: Vec::new(),
        plugin_config: Default::default(),
        context_policy: Default::default(),
        tool_patterns: Vec::new(),
        model_candidates: Vec::new(),
        // Managed identity/wire fields (config-plane authoring metadata); unused here.
        ..Default::default()
    }
}

/// The seeded system prompt for the management assistant. It is authored into the
/// assistant's ordinary `AgentConfig` like any agent's instructions (D4) — no locked
/// prompt, no policy overlay.
pub const ADMIN_ASSISTANT_INSTRUCTIONS: &str = "\
You are the platform's management assistant. You do two things: (1) turn an operator's \
plain-English intent into a valid agent configuration, and (2) act as an in-console user \
manual — answer 'how do I…' / 'what is…' questions about the console.

For a how-to or concept question, call `admin_explain_console` (with a `topic`, or with no \
topic to see the topic list) and answer from what it returns — do not guess how the \
console works. If the operator is on a specific page, explain that page's topic. Keep help \
answers short and point them at where to click.

To AUTHOR an agent, follow this WORKFLOW (in order):
1. ALWAYS call `admin_get_platform_capabilities` first. It returns the available tools \
and the installable plugins, each WITH its `config_schema` (a JSON Schema for that \
plugin's config section). Never invent a tool or plugin id that is not listed.
2. Call `admin_draft_agent` once to author the whole config from intent — id, \
instructions, tools, tool_overrides, and any plugin sections together. It saves an \
unpublished draft and validates it.
3. If a draft fails to validate, read the error and fix it with `admin_patch_agent`; \
use `admin_validate_agent` to confirm. Iterate until it compiles cleanly.
4. Briefly tell the operator what you drafted and the trade-offs. NEVER publish — \
publishing is the operator's decision in the console.

AUTHORING RULES:
- Plugin sections (e.g. `state_machine`, `permission`, `compact`, `memory`) MUST conform \
EXACTLY to that plugin's `config_schema` from step 1. Read the schema; match its field \
names, nesting, and enums precisely — do not guess the shape.
- `tool_overrides` shape each per entry: `target` is the tool's id; set `alias` to rename \
it for the model and/or `description` to re-describe it. Use it when the operator asks to \
rename or re-explain a tool.
- Permissions: prefer least privilege. If the operator wants approval or bans, put it in \
the `permission` section (a `default_behavior` of `ask`/`deny`, and/or ordered `rules`).
- Select the minimum tools the task needs; omit tools entirely when none are needed.
- Only pin a model if the operator names one; otherwise leave it auto-bound.
- You can fill in EVERY part of a config, matching the manual editor: besides tools and \
plugins you may set `mcp_servers`, `skills`, `multiagent`, and `metadata`, and BIND \
data-plane `resources` (memory stores, files, git repos, skills) onto the agent.
- Resource binding: each `resources` entry is `{ kind (memory_store|file|\
github_repository|skill), resource_id, mount_path?, access? (read_only|read_write, \
default read_write), instructions? }`. Prefer a `resource_id` from the capability view \
(its `memory_stores`/`skills`/etc.). If the operator explicitly gives you a specific \
resource_id, bind it as given — they have confirmed it. Only ask when the operator is \
vague about which resource. Omit `mount_path` to accept the per-kind default. On \
`admin_patch_agent`, a present `resources` array REPLACES the agent's whole binding set.";

/// A cap on a single plugin-config section, so a draft/patch cannot be used to stuff
/// an unbounded blob into a config (D4: size-bounded).
const MAX_PLUGIN_CONFIG_BYTES: usize = 64 * 1024;

// ---- Ports the host implements (adapters over real platform state) --------------

/// A redacted, **org-shared** snapshot of what the platform can do (D4). The host
/// implements this over the shared provider/model catalog, the advertised tool ids,
/// and the plugin capabilities. It never crosses scope to read a tenant's private
/// detail and never carries a key, credential, or header. It is async because the
/// LIVE snapshot is read from durable stores (the catalog repo, the config plane).
#[async_trait]
pub trait CapabilityReader: Send + Sync {
    async fn capabilities(&self) -> PlatformCapabilities;
}

/// A read port over the DATA-PLANE resource inventory (memory stores + skills), which
/// live outside the control plane. The host implements it over the memory-store
/// registry and the skill store; keeping it a port lets `awaken-control` compose a LIVE
/// [`CapabilityReader`] without depending on any data-plane crate. Carries only
/// ids/names — never a secret.
#[async_trait]
pub trait ResourceInventory: Send + Sync {
    /// Ids of the memory stores the platform can bind onto an agent.
    async fn memory_stores(&self) -> Vec<String>;
    /// Ids of the skills discoverable at run time.
    async fn skills(&self) -> Vec<String>;
}

/// Validate a drafted [`AgentConfig`] exactly as `/v1/config/agents/validate` does —
/// a compile dry-run against the target scope's tool catalog (fail-closed on an
/// unknown tool). The host implements it over its `ConfigService`.
pub trait DraftValidator: Send + Sync {
    /// `Ok(())` if the draft compiles; `Err(message)` with the compile error.
    fn validate(&self, draft: &AgentConfig) -> Result<(), String>;
}

/// A neutral, transport-shaped resource-binding spec (ADR-0038): one resource an agent
/// mounts, authored by the assistant from the flattened `resources` intent. It is a
/// leaf value in THIS crate — deliberately NOT the config-resolver's `ResourceBinding`
/// — so the assistant crate stays a leaf that names only the neutral tool contract; the
/// host's [`DraftStore`] adapter maps this onto the real binding (kind/access enums +
/// per-kind default mount path). `kind` is `memory_store`/`file`/`github_repository`/
/// `skill`; `access` is `read_only`/`read_write` (defaults to `read_write`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceSpec {
    pub kind: String,
    pub resource_id: String,
    #[serde(default)]
    pub mount_path: Option<String>,
    #[serde(default)]
    pub access: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
}

/// Persist and read back **unpublished** draft agent configs (ADR-0052). The host
/// implements this over the same config plane the editor's Save uses, in the tenant/
/// default scope. A draft *is* an unpublished config agent — persisting it here makes
/// it appear in the console's agent list, still awaiting the operator's publish. It also
/// carries the SEPARATE data-plane resource bindings (ADR-0038), which live in their own
/// store; the assistant authors both through this one port so a single tool call fills
/// in a whole agent — config plus its mounted resources.
#[async_trait]
pub trait DraftStore: Send + Sync {
    /// Persist an UNPUBLISHED draft agent config (same effect as the editor's Save).
    async fn put(&self, draft: &AgentConfig) -> Result<(), String>;
    /// Read a persisted draft agent config back by id (`None` if absent).
    async fn get(&self, id: &str) -> Result<Option<AgentConfig>, String>;
    /// Replace the whole set of resource bindings for `agent_id` (data-plane store).
    async fn put_resources(
        &self,
        agent_id: &str,
        resources: Vec<ResourceSpec>,
    ) -> Result<(), String>;
    /// Read back the agent's resource bindings (empty when none are bound).
    async fn get_resources(&self, agent_id: &str) -> Result<Vec<ResourceSpec>, String>;
}

/// A structured record of one management tool invocation (ADR-0052 D6). Emitted on
/// **every** call. Carries only a short, non-secret summary — never the full arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminAuditEvent {
    pub tool: String,
    pub call_id: String,
    pub summary: String,
}

/// Where management tool-call audit records go (ADR-0052 D6). The default
/// [`TracingAuditSink`] logs to the `awaken::admin_audit` target; a deployment can
/// inject its own (e.g. a durable audit store) — this is also the defense-in-depth
/// seam, since every privileged call passes through it.
pub trait AuditSink: Send + Sync {
    fn record(&self, event: AdminAuditEvent);
}

/// The default audit sink: emit a structured `tracing` event on the
/// `awaken::admin_audit` target (ADR-0052 D6).
pub struct TracingAuditSink;

impl AuditSink for TracingAuditSink {
    fn record(&self, event: AdminAuditEvent) {
        tracing::info!(
            target: "awaken::admin_audit",
            tool = %event.tool,
            call_id = %event.call_id,
            summary = %event.summary,
            "management tool call",
        );
    }
}

/// The redacted capability snapshot returned by [`CapabilityReader`]. Every field is
/// a list of ids/names — no secrets, endpoints, or headers — so it is redacted by
/// construction (D4).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    /// Ids of agents already published in the org.
    pub agents: Vec<String>,
    /// Model ids offered by the shared provider catalog.
    pub models: Vec<String>,
    /// Provider ids in the shared catalog.
    pub providers: Vec<String>,
    /// Tool ids an ordinary agent may name (the global catalog — not the admin tools).
    pub tools: Vec<String>,
    /// Plugins available to compose onto an agent, with their config schema keys.
    pub plugins: Vec<PluginInfo>,
    /// Skill ids discoverable at run time.
    pub skills: Vec<String>,
    /// Connected MCP server ids.
    pub mcp_servers: Vec<String>,
    /// Ids of the memory stores an agent may bind (data-plane inventory).
    #[serde(default)]
    pub memory_stores: Vec<String>,
}

/// One composable plugin and the config-section keys it reads, plus its full JSON
/// Schema so the assistant authors a schema-conformant section instead of guessing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub schema_keys: Vec<String>,
    /// The plugin's `plugin_config` JSON Schema (shape of its config section). Present
    /// when the platform published one; the assistant conforms to it exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
}

// ---- The four descriptors --------------------------------------------------------

/// The four management tool descriptors (D3). These are handed to the host's
/// scope-keyed catalog so they are nameable **only** in the reserved scope; a config
/// in any other scope that names one hits `UnknownTool` at compile (fail-closed).
#[must_use]
pub fn admin_tool_descriptors() -> Vec<ToolDescriptor> {
    // Note: no descriptor uses `additionalProperties` — Gemini's function-declaration
    // schema rejects it. Open `object` fields (plugin_config, context_policy, patch)
    // are declared as a bare `{ "type": "object" }` with no closure clause.
    vec![
        ToolDescriptor::pinned(
            "admin",
            CAPABILITIES_TOOL,
            "List the platform's LIVE building blocks: available models, providers, \
             tools, plugins, skills, MCP servers, memory stores, and the ids of agents \
             already published in the org (redacted; no secrets). Reference these \
             existing blocks when authoring — call this before proposing a config.",
            serde_json::json!({ "type": "object", "properties": {} }),
        ),
        ToolDescriptor::pinned(
            "admin",
            CREATE_DRAFT_TOOL,
            "Author a full agent configuration from the operator's intent and SAVE it \
             as an unpublished draft. Accepts the WHOLE config as flat fields: \
             instructions, model, tools, tool_overrides, plugin_config, mcp_servers, \
             skills, multiagent, metadata, and `resources` (data-plane bindings — \
             memory stores, files, git repos, skills — by resource_id). Validates \
             before saving; on a validation error nothing is saved. Never publishes.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The new agent's id." },
                    "instructions": { "type": "string", "description": "The agent's system prompt." },
                    "name": { "type": "string" },
                    "description": { "type": "string" },
                    "model": {
                        "type": "string",
                        "description": "Pin a model id; omit to auto-bind at publish."
                    },
                    "max_steps": { "type": "integer", "minimum": 1 },
                    "tool_ids": { "type": "array", "items": { "type": "string" } },
                    "tool_patterns": { "type": "array", "items": { "type": "string" } },
                    "tool_overrides": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "target": { "type": "string" },
                                "alias": { "type": "string" },
                                "description": { "type": "string" },
                                "defer": { "type": "boolean" }
                            },
                            "required": ["target"]
                        }
                    },
                    "plugin_config": {
                        "type": "object",
                        "description": "Per-plugin config sections keyed by plugin id \
                                        (permission/state_machine/compact/memory)."
                    },
                    "context_policy": { "type": "object" },
                    "mcp_servers": {
                        "type": "array",
                        "items": { "type": "object" },
                        "description": "MCP server sections to attach (each an object \
                                        conforming to the MCP config shape). Reference \
                                        only server ids listed in capabilities."
                    },
                    "skills": {
                        "type": "array",
                        "items": { "type": "object" },
                        "description": "Skill config sections to attach."
                    },
                    "multiagent": {
                        "type": "object",
                        "description": "The multiagent/orchestration section, if any."
                    },
                    "metadata": {
                        "type": "object",
                        "description": "Free-form string→string metadata for the agent."
                    },
                    "resources": {
                        "type": "array",
                        "description": "Data-plane resources to mount onto the agent \
                                        (memory stores, files, git repos, skills). Bind \
                                        only resource_ids present in the capability view.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": {
                                    "type": "string",
                                    "enum": ["memory_store", "file", "github_repository", "skill"],
                                    "description": "The resource kind to bind."
                                },
                                "resource_id": {
                                    "type": "string",
                                    "description": "Id of the resource from the capability view."
                                },
                                "mount_path": {
                                    "type": "string",
                                    "description": "Where it mounts (defaulted per kind if omitted)."
                                },
                                "access": {
                                    "type": "string",
                                    "enum": ["read_only", "read_write"],
                                    "description": "Access mode (default read_write)."
                                },
                                "instructions": { "type": "string" }
                            },
                            "required": ["kind", "resource_id"]
                        }
                    }
                },
                "required": ["id", "instructions"]
            }),
        ),
        ToolDescriptor::pinned(
            "admin",
            PATCH_TOOL,
            "Incrementally refine a SAVED draft: apply only the fields present in \
             `patch` (same flat field set as admin_draft_agent — instructions, model, \
             tools, plugin_config, mcp_servers, skills, multiagent, metadata, resources) \
             to the stored draft, re-validate, and save. plugin_config sections merge by \
             key; a present `resources` array REPLACES the whole binding set (an absent \
             one leaves bindings untouched). Never publishes.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The saved draft's id." },
                    "patch": {
                        "type": "object",
                        "description": "The subset of config fields to change."
                    }
                },
                "required": ["id", "patch"]
            }),
        ),
        ToolDescriptor::pinned(
            "admin",
            VALIDATE_TOOL,
            "Validate a saved draft agent configuration by id (does it compile?). \
             Read-only.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The saved draft's id." }
                },
                "required": ["id"]
            }),
        ),
        ToolDescriptor::pinned(
            "admin",
            EXPLAIN_TOOL,
            "Explain how to use this console (read-only user manual). Call with a `topic` \
             to get a curated explanation (what/why/where/how/gotchas); call with no \
             topic to list the available topics. Use it to answer 'how do I…' questions.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "topic": { "type": "string", "description": "A help topic id (omit to list topics)." }
                }
            }),
        ),
    ]
}

/// The four executable management tools, erased for runtime registration (D3). The
/// host registers these globally (the runtime tool registry stays global); the
/// compile-time scope projection is what fences them to the reserved scope.
#[must_use]
pub fn admin_tools(
    reader: Arc<dyn CapabilityReader>,
    validator: Arc<dyn DraftValidator>,
    store: Arc<dyn DraftStore>,
    audit: Arc<dyn AuditSink>,
) -> Vec<Arc<dyn RawTool>> {
    vec![
        Arc::new(GetPlatformCapabilities {
            reader,
            audit: audit.clone(),
        }),
        Arc::new(DraftAgent {
            validator: validator.clone(),
            store: store.clone(),
            audit: audit.clone(),
        }),
        Arc::new(PatchAgent {
            validator: validator.clone(),
            store: store.clone(),
            audit: audit.clone(),
        }),
        Arc::new(ValidateAgent {
            validator,
            store,
            audit: audit.clone(),
        }),
        Arc::new(ExplainConsole { audit }),
    ]
}

// ---- Tool 5: admin_explain_console (read-only in-console user manual) ------------

struct ExplainConsole {
    audit: Arc<dyn AuditSink>,
}

#[derive(Debug, Deserialize)]
struct ExplainArgs {
    #[serde(default)]
    topic: Option<String>,
}

#[async_trait]
impl RawTool for ExplainConsole {
    fn id(&self) -> &str {
        EXPLAIN_TOOL
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        // Tolerant of a missing/empty arg object — an index request is well-formed.
        let args: ExplainArgs =
            serde_json::from_value(call.arguments.clone()).unwrap_or(ExplainArgs { topic: None });
        audit(
            &self.audit,
            EXPLAIN_TOOL,
            &call.call_id,
            format!(
                "explain console `{}`",
                args.topic.as_deref().unwrap_or("(index)")
            ),
        );
        Ok(ToolOutput::ok(
            call.call_id,
            console_help::explain(args.topic.as_deref()).to_string(),
        ))
    }
}

/// Emit the audit record for one management tool call (ADR-0052 D6).
fn audit(sink: &Arc<dyn AuditSink>, tool: &str, call_id: &str, summary: impl Into<String>) {
    sink.record(AdminAuditEvent {
        tool: tool.to_string(),
        call_id: call_id.to_string(),
        summary: summary.into(),
    });
}

/// Size-bound every plugin-config section (D4): never let a draft absorb an unbounded
/// blob. Returns the first offending key's error message.
fn check_plugin_config_size(
    plugin_config: &BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    for (key, section) in plugin_config {
        let len = serde_json::to_string(section).map(|s| s.len()).unwrap_or(0);
        if len > MAX_PLUGIN_CONFIG_BYTES {
            return Err(format!(
                "plugin config section `{key}` is {len} bytes, over the {MAX_PLUGIN_CONFIG_BYTES}-byte limit"
            ));
        }
    }
    Ok(())
}

/// `plugin_ids` = the deduped keys of `plugin_config`, in deterministic order.
fn plugin_ids_of(plugin_config: &BTreeMap<String, serde_json::Value>) -> Vec<String> {
    plugin_config.keys().cloned().collect()
}

/// Validate a config, then persist it as an unpublished draft, and emit the saved
/// config plus a short pointer line. Fail-closed: a validation error does NOT persist.
async fn validate_persist_emit(
    call_id: String,
    config: AgentConfig,
    validator: &Arc<dyn DraftValidator>,
    store: &Arc<dyn DraftStore>,
) -> Result<ToolOutput, ToolError> {
    if let Err(error) = validator.validate(&config) {
        return Ok(ToolOutput::error(
            call_id,
            format!("draft does not validate, not saved: {error}"),
        ));
    }
    if let Err(error) = store.put(&config).await {
        return Ok(ToolOutput::error(
            call_id,
            format!("draft validated but could not be saved: {error}"),
        ));
    }
    let note = format!(
        "Saved draft `{}` (unpublished). Publish it from the console when ready.",
        config.id
    );
    let body = serde_json::json!({ "config": config, "note": note });
    Ok(ToolOutput::ok(call_id, body.to_string()))
}

/// Bind the SEPARATE data-plane resources after the config was saved. Fail-safe: if the
/// config save already errored (`out.is_error`) or the caller passed `None` (nothing to
/// change), resources are left untouched; a present set REPLACES the agent's bindings.
async fn persist_resources_after(
    out: ToolOutput,
    agent_id: &str,
    resources: Option<Vec<ResourceSpec>>,
    store: &Arc<dyn DraftStore>,
) -> Result<ToolOutput, ToolError> {
    if out.is_error {
        return Ok(out);
    }
    let Some(resources) = resources else {
        return Ok(out);
    };
    if let Err(error) = store.put_resources(agent_id, resources).await {
        return Ok(ToolOutput::error(
            out.call_id,
            format!("draft saved but its resources could not be bound: {error}"),
        ));
    }
    Ok(out)
}

// ---- Tool 1: admin_get_platform_capabilities ------------------------------------

struct GetPlatformCapabilities {
    reader: Arc<dyn CapabilityReader>,
    audit: Arc<dyn AuditSink>,
}

#[async_trait]
impl RawTool for GetPlatformCapabilities {
    fn id(&self) -> &str {
        CAPABILITIES_TOOL
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        audit(
            &self.audit,
            CAPABILITIES_TOOL,
            &call.call_id,
            "list platform capabilities",
        );
        let caps = self.reader.capabilities().await;
        let content = serde_json::to_string(&caps)
            .map_err(|e| ToolError::Execution(format!("serialize capabilities: {e}")))?;
        Ok(ToolOutput::ok(call.call_id, content))
    }
}

// ---- Tool 2: admin_draft_agent --------------------------------------------------

/// The flattened full-config input for [`CREATE_DRAFT_TOOL`]. Every field but `id`
/// and `instructions` is optional; `model` pins a model id (omit → `Auto`).
#[derive(Debug, Deserialize)]
struct DraftArgs {
    id: String,
    instructions: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_steps: Option<usize>,
    #[serde(default)]
    tool_ids: Vec<String>,
    #[serde(default)]
    tool_patterns: Vec<String>,
    #[serde(default)]
    tool_overrides: Vec<ToolOverride>,
    #[serde(default)]
    plugin_config: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    context_policy: Option<ContextPolicy>,
    #[serde(default)]
    mcp_servers: Vec<serde_json::Value>,
    #[serde(default)]
    skills: Vec<serde_json::Value>,
    #[serde(default)]
    multiagent: Option<serde_json::Value>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    resources: Vec<ResourceSpec>,
}

struct DraftAgent {
    validator: Arc<dyn DraftValidator>,
    store: Arc<dyn DraftStore>,
    audit: Arc<dyn AuditSink>,
}

#[async_trait]
impl RawTool for DraftAgent {
    fn id(&self) -> &str {
        CREATE_DRAFT_TOOL
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let args: DraftArgs = match serde_json::from_value(call.arguments.clone()) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolOutput::error(
                    call.call_id,
                    format!("invalid arguments: {e}"),
                ));
            }
        };
        audit(
            &self.audit,
            CREATE_DRAFT_TOOL,
            &call.call_id,
            format!("draft agent `{}`", args.id),
        );
        if let Err(error) = check_plugin_config_size(&args.plugin_config) {
            return Ok(ToolOutput::error(call.call_id, error));
        }
        // `model` pins a concrete id; absent → Auto (resolve at publish, ADR-0052 D5).
        let model_binding = args
            .model
            .map(|m| ModelSelection::pinned("default", m, "default"))
            .unwrap_or(ModelSelection::Auto);
        let plugin_ids = plugin_ids_of(&args.plugin_config);
        let id = args.id.clone();
        let resources = args.resources;
        let config = AgentConfig {
            id: args.id,
            instructions: args.instructions,
            max_steps: args.max_steps.unwrap_or(8),
            model_binding,
            tool_ids: args.tool_ids,
            plugin_ids,
            plugin_config: args.plugin_config,
            context_policy: args.context_policy.unwrap_or_default(),
            tool_patterns: args.tool_patterns,
            model_candidates: Vec::new(),
            name: args.name,
            description: args.description,
            tool_overrides: args.tool_overrides,
            mcp_servers: args.mcp_servers,
            skills: args.skills,
            multiagent: args.multiagent,
            metadata: args.metadata,
        };
        let out = validate_persist_emit(call.call_id, config, &self.validator, &self.store).await?;
        // Resources are a SEPARATE store: only bind them once the config validated + was
        // saved (a validation/save error already short-circuited above). On create we
        // bind only when the operator named at least one resource.
        let to_bind = (!resources.is_empty()).then_some(resources);
        persist_resources_after(out, &id, to_bind, &self.store).await
    }
}

// ---- Tool 3: admin_patch_agent --------------------------------------------------

#[derive(Debug, Deserialize)]
struct PatchArgs {
    id: String,
    patch: PatchFields,
}

/// The subset of flattened config fields a patch may change. An absent field is left
/// untouched on the stored draft; a present field replaces it (plugin_config merges
/// by key rather than replacing the whole map).
#[derive(Debug, Default, Deserialize)]
struct PatchFields {
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_steps: Option<usize>,
    #[serde(default)]
    tool_ids: Option<Vec<String>>,
    #[serde(default)]
    tool_patterns: Option<Vec<String>>,
    #[serde(default)]
    tool_overrides: Option<Vec<ToolOverride>>,
    #[serde(default)]
    plugin_config: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    context_policy: Option<ContextPolicy>,
    #[serde(default)]
    mcp_servers: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    skills: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    multiagent: Option<serde_json::Value>,
    #[serde(default)]
    metadata: Option<BTreeMap<String, String>>,
    /// When present, REPLACES the agent's whole resource-binding set (data-plane store);
    /// when absent, the existing bindings are left untouched.
    #[serde(default)]
    resources: Option<Vec<ResourceSpec>>,
}

struct PatchAgent {
    validator: Arc<dyn DraftValidator>,
    store: Arc<dyn DraftStore>,
    audit: Arc<dyn AuditSink>,
}

#[async_trait]
impl RawTool for PatchAgent {
    fn id(&self) -> &str {
        PATCH_TOOL
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let args: PatchArgs = match serde_json::from_value(call.arguments.clone()) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolOutput::error(
                    call.call_id,
                    format!("invalid arguments: {e}"),
                ));
            }
        };
        audit(
            &self.audit,
            PATCH_TOOL,
            &call.call_id,
            format!("patch agent `{}`", args.id),
        );
        // Read the persisted draft; a patch targets an existing draft (fail-closed).
        let mut config = match self.store.get(&args.id).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                return Ok(ToolOutput::error(
                    call.call_id,
                    format!("no saved draft `{}` to patch", args.id),
                ));
            }
            Err(e) => {
                return Ok(ToolOutput::error(
                    call.call_id,
                    format!("could not read draft `{}`: {e}", args.id),
                ));
            }
        };
        // Apply only the fields present in the patch.
        let patch = args.patch;
        if let Some(v) = patch.instructions {
            config.instructions = v;
        }
        if let Some(v) = patch.name {
            config.name = Some(v);
        }
        if let Some(v) = patch.description {
            config.description = Some(v);
        }
        if let Some(m) = patch.model {
            config.model_binding = ModelSelection::pinned("default", m, "default");
        }
        if let Some(v) = patch.max_steps {
            config.max_steps = v;
        }
        if let Some(v) = patch.tool_ids {
            config.tool_ids = v;
        }
        if let Some(v) = patch.tool_patterns {
            config.tool_patterns = v;
        }
        if let Some(v) = patch.tool_overrides {
            config.tool_overrides = v;
        }
        if let Some(cp) = patch.context_policy {
            config.context_policy = cp;
        }
        if let Some(v) = patch.mcp_servers {
            config.mcp_servers = v;
        }
        if let Some(v) = patch.skills {
            config.skills = v;
        }
        if let Some(v) = patch.multiagent {
            config.multiagent = Some(v);
        }
        if let Some(v) = patch.metadata {
            config.metadata = v;
        }
        if let Some(sections) = patch.plugin_config {
            // Merge by key: each provided section is inserted/overwritten, and the
            // rest of the map is preserved (incremental refine, not wholesale replace).
            for (key, section) in sections {
                config.plugin_config.insert(key, section);
            }
        }
        // Re-derive plugin_ids from the merged plugin_config so the two never drift.
        config.plugin_ids = plugin_ids_of(&config.plugin_config);
        if let Err(error) = check_plugin_config_size(&config.plugin_config) {
            return Ok(ToolOutput::error(call.call_id, error));
        }
        let id = config.id.clone();
        let resources = patch.resources;
        let out = validate_persist_emit(call.call_id, config, &self.validator, &self.store).await?;
        // A present `resources` array REPLACES the whole binding set (even an empty array
        // clears it); an absent one leaves the existing bindings untouched.
        persist_resources_after(out, &id, resources, &self.store).await
    }
}

// ---- Tool 4: admin_validate_agent -----------------------------------------------

#[derive(Debug, Deserialize)]
struct ValidateArgs {
    id: String,
}

struct ValidateAgent {
    validator: Arc<dyn DraftValidator>,
    store: Arc<dyn DraftStore>,
    audit: Arc<dyn AuditSink>,
}

#[async_trait]
impl RawTool for ValidateAgent {
    fn id(&self) -> &str {
        VALIDATE_TOOL
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let args: ValidateArgs = match serde_json::from_value(call.arguments.clone()) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolOutput::error(
                    call.call_id,
                    format!("invalid arguments: {e}"),
                ));
            }
        };
        audit(
            &self.audit,
            VALIDATE_TOOL,
            &call.call_id,
            format!("validate draft `{}`", args.id),
        );
        let draft = match self.store.get(&args.id).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                return Ok(ToolOutput::error(
                    call.call_id,
                    format!("no saved draft `{}` to validate", args.id),
                ));
            }
            Err(e) => {
                return Ok(ToolOutput::error(
                    call.call_id,
                    format!("could not read draft `{}`: {e}", args.id),
                ));
            }
        };
        let result = match self.validator.validate(&draft) {
            Ok(()) => serde_json::json!({ "valid": true }),
            Err(error) => serde_json::json!({ "valid": false, "error": error }),
        };
        Ok(ToolOutput::ok(call.call_id, result.to_string()))
    }
}

#[cfg(test)]
mod tests;
