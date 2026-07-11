//! The management ("admin") assistant's tools — ADR-0052.
//!
//! This is the platform's FAB-equivalent: an ordinary agent (authored, compiled,
//! published like any other) whose *privilege* is expressed entirely by the scope it
//! lives in and the tools visible in that scope (ADR-0052 D1–D3), not by a special
//! agent type or a field on any value object. This crate owns only the four
//! **read-only** management tools and their descriptors, plus the seeded system
//! prompt; the scope fence, seeding, model auto-binding, and audit live in the host
//! (D2/D3/D5/D6).
//!
//! The four tools are capability-access only — read-only, redacted, never-publish —
//! so there is no untrusted execution and no sandbox axis (D4). Safety is a property
//! of these executors (they read only the org-shared, redacted view and never
//! write), not of a cage. There is deliberately **no publish tool**: publication is a
//! console action, never an LLM tool call.
//!
//! The tools reach real platform state through two ports the host implements
//! ([`CapabilityReader`], [`DraftValidator`]) — so this crate stays a leaf that names
//! only the neutral tool contract and the config aggregate, never the server.

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use awaken_config_store::{AgentConfig, ModelSelection};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

/// The four management tool ids. They are namespaced `admin_*` and are only ever
/// nameable in the reserved scope (the fence is the scope-keyed catalog projection,
/// ADR-0052 D3 — enforced in the host, not here).
pub const CAPABILITIES_TOOL: &str = "admin_get_platform_capabilities";
pub const CREATE_DRAFT_TOOL: &str = "admin_create_agent_draft";
pub const SET_PLUGIN_TOOL: &str = "admin_set_plugin_config";
pub const VALIDATE_TOOL: &str = "admin_validate_agent";

/// The reserved agent id the management assistant is published under. It is a
/// deliberately un-tenant-like id, seeded once into the reserved scope (ADR-0052 D2).
pub const ADMIN_ASSISTANT_AGENT_ID: &str = "__admin_assistant";

/// All four management tool ids, in advertised order.
#[must_use]
pub fn admin_tool_ids() -> Vec<String> {
    vec![
        CAPABILITIES_TOOL.to_string(),
        CREATE_DRAFT_TOOL.to_string(),
        SET_PLUGIN_TOOL.to_string(),
        VALIDATE_TOOL.to_string(),
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
/// prompt, no policy overlay: given read-only tools, the prompt needs no protection.
pub const ADMIN_ASSISTANT_INSTRUCTIONS: &str = "\
You are the platform's management assistant. You help an organization's operators \
author and refine agent configurations for this platform. Use \
`admin_get_platform_capabilities` to see the available models, providers, tools, and \
plugins before proposing anything. Use `admin_create_agent_draft` to draft a new \
agent configuration from the operator's intent, `admin_set_plugin_config` to attach a \
plugin's configuration section to a draft, and `admin_validate_agent` to check that a \
draft compiles. You never publish: publishing a configuration is the operator's \
decision in the console, not something you do. Only propose configurations that \
validate cleanly, and explain the trade-offs of what you propose.";

/// A cap on a single plugin-config section, so `admin_set_plugin_config` cannot be
/// used to stuff an unbounded blob into a draft (D4: size-bounded).
const MAX_PLUGIN_CONFIG_BYTES: usize = 64 * 1024;

// ---- Ports the host implements (adapters over real platform state) --------------

/// A redacted, **org-shared** snapshot of what the platform can do (D4). The host
/// implements this over the shared provider/model catalog, the advertised tool ids,
/// and the plugin capabilities. It never crosses scope to read a tenant's private
/// detail and never carries a key, credential, or header.
pub trait CapabilityReader: Send + Sync {
    fn capabilities(&self) -> PlatformCapabilities;
}

/// Validate a drafted [`AgentConfig`] exactly as `/v1/config/agents/validate` does —
/// a compile dry-run against the target scope's tool catalog (fail-closed on an
/// unknown tool). The host implements it over its `ConfigService`.
pub trait DraftValidator: Send + Sync {
    /// `Ok(())` if the draft compiles; `Err(message)` with the compile error.
    fn validate(&self, draft: &AgentConfig) -> Result<(), String>;
}

/// A structured record of one management tool invocation (ADR-0052 D6). Emitted on
/// **every** call, including the read-only draft/validate tools. Carries only a short,
/// non-secret summary — never the full arguments.
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
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
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
}

/// One composable plugin and the config-section keys it reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub schema_keys: Vec<String>,
}

// ---- The four descriptors --------------------------------------------------------

/// The four management tool descriptors (D3). These are handed to the host's
/// scope-keyed catalog so they are nameable **only** in the reserved scope; a config
/// in any other scope that names one hits `UnknownTool` at compile (fail-closed).
#[must_use]
pub fn admin_tool_descriptors() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor::pinned(
            "admin",
            CAPABILITIES_TOOL,
            "List the platform's available models, providers, tools, plugins, skills, \
             and MCP servers (redacted; no secrets). Call this before proposing a config.",
            serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        ToolDescriptor::pinned(
            "admin",
            CREATE_DRAFT_TOOL,
            "Draft a new agent configuration from the operator's intent. Returns an \
             unpublished draft (model auto-bound). Never publishes.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The new agent's id." },
                    "instructions": { "type": "string", "description": "The agent's system prompt." },
                    "tool_ids": { "type": "array", "items": { "type": "string" } },
                    "plugin_ids": { "type": "array", "items": { "type": "string" } },
                    "max_steps": { "type": "integer", "minimum": 1 }
                },
                "required": ["id", "instructions"]
            }),
        ),
        ToolDescriptor::pinned(
            "admin",
            SET_PLUGIN_TOOL,
            "Attach or replace a plugin's configuration section on a draft, and \
             validate the result. Never publishes.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "draft": { "type": "object", "description": "The draft agent config to amend." },
                    "plugin_id": { "type": "string" },
                    "config": { "type": "object", "description": "The plugin's config section." }
                },
                "required": ["draft", "plugin_id", "config"]
            }),
        ),
        ToolDescriptor::pinned(
            "admin",
            VALIDATE_TOOL,
            "Validate a draft agent configuration (does it compile?). Read-only.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "draft": { "type": "object", "description": "The draft agent config to validate." }
                },
                "required": ["draft"]
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
    audit: Arc<dyn AuditSink>,
) -> Vec<Arc<dyn RawTool>> {
    vec![
        Arc::new(GetPlatformCapabilities {
            reader,
            audit: audit.clone(),
        }),
        Arc::new(CreateAgentDraft {
            audit: audit.clone(),
        }),
        Arc::new(SetPluginConfig {
            validator: validator.clone(),
            audit: audit.clone(),
        }),
        Arc::new(ValidateAgent { validator, audit }),
    ]
}

/// Emit the audit record for one management tool call (ADR-0052 D6).
fn audit(sink: &Arc<dyn AuditSink>, tool: &str, call_id: &str, summary: impl Into<String>) {
    sink.record(AdminAuditEvent {
        tool: tool.to_string(),
        call_id: call_id.to_string(),
        summary: summary.into(),
    });
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
        let caps = self.reader.capabilities();
        let content = serde_json::to_string(&caps)
            .map_err(|e| ToolError::Execution(format!("serialize capabilities: {e}")))?;
        Ok(ToolOutput::ok(call.call_id, content))
    }
}

// ---- Tool 2: admin_create_agent_draft -------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateDraftArgs {
    id: String,
    instructions: String,
    #[serde(default)]
    tool_ids: Vec<String>,
    #[serde(default)]
    plugin_ids: Vec<String>,
    max_steps: Option<usize>,
}

struct CreateAgentDraft {
    audit: Arc<dyn AuditSink>,
}

#[async_trait]
impl RawTool for CreateAgentDraft {
    fn id(&self) -> &str {
        CREATE_DRAFT_TOOL
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let args: CreateDraftArgs = match serde_json::from_value(call.arguments.clone()) {
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
        // A draft: model auto-bound (ADR-0052 D5), never published. This tool only
        // returns data; the operator publishes from the console.
        let draft = AgentConfig {
            id: args.id,
            instructions: args.instructions,
            max_steps: args.max_steps.unwrap_or(8),
            model_binding: ModelSelection::Auto,
            tool_ids: args.tool_ids,
            plugin_ids: args.plugin_ids,
            plugin_config: Default::default(),
            context_policy: Default::default(),
            tool_patterns: Vec::new(),
            model_candidates: Vec::new(),
            ..Default::default()
        };
        emit_draft(call.call_id, &draft)
    }
}

// ---- Tool 3: admin_set_plugin_config --------------------------------------------

#[derive(Debug, Deserialize)]
struct SetPluginArgs {
    draft: AgentConfig,
    plugin_id: String,
    config: serde_json::Value,
}

struct SetPluginConfig {
    validator: Arc<dyn DraftValidator>,
    audit: Arc<dyn AuditSink>,
}

#[async_trait]
impl RawTool for SetPluginConfig {
    fn id(&self) -> &str {
        SET_PLUGIN_TOOL
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let args: SetPluginArgs = match serde_json::from_value(call.arguments.clone()) {
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
            SET_PLUGIN_TOOL,
            &call.call_id,
            format!("set plugin `{}`", args.plugin_id),
        );
        // Size-bound the section (D4): never let a draft absorb an unbounded blob.
        let section_len = serde_json::to_string(&args.config)
            .map(|s| s.len())
            .unwrap_or(0);
        if section_len > MAX_PLUGIN_CONFIG_BYTES {
            return Ok(ToolOutput::error(
                call.call_id,
                format!(
                    "plugin config section is {section_len} bytes, over the {MAX_PLUGIN_CONFIG_BYTES}-byte limit"
                ),
            ));
        }
        let mut draft = args.draft;
        if !draft.plugin_ids.contains(&args.plugin_id) {
            draft.plugin_ids.push(args.plugin_id.clone());
        }
        draft.plugin_config.insert(args.plugin_id, args.config);
        // Attaching a section validates the result, so an incompatible section is
        // caught here rather than at a later publish.
        if let Err(error) = self.validator.validate(&draft) {
            return Ok(ToolOutput::error(
                call.call_id,
                format!("draft does not validate after attaching the plugin config: {error}"),
            ));
        }
        emit_draft(call.call_id, &draft)
    }
}

// ---- Tool 4: admin_validate_agent -----------------------------------------------

#[derive(Debug, Deserialize)]
struct ValidateArgs {
    draft: AgentConfig,
}

struct ValidateAgent {
    validator: Arc<dyn DraftValidator>,
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
            format!("validate draft `{}`", args.draft.id),
        );
        let result = match self.validator.validate(&args.draft) {
            Ok(()) => serde_json::json!({ "valid": true }),
            Err(error) => serde_json::json!({ "valid": false, "error": error }),
        };
        Ok(ToolOutput::ok(call.call_id, result.to_string()))
    }
}

/// Serialize a draft config as a tool result. A draft is always model-visible data;
/// it is never written or published by these tools.
fn emit_draft(call_id: String, draft: &AgentConfig) -> Result<ToolOutput, ToolError> {
    let content = serde_json::to_string(draft)
        .map_err(|e| ToolError::Execution(format!("serialize draft: {e}")))?;
    Ok(ToolOutput::ok(call_id, content))
}

#[cfg(test)]
mod tests;
