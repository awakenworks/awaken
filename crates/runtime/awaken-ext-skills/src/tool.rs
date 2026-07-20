//! The two skill-specific tools: `list_skills` (discover) and `Skill` (activate).
//!
//! These are the whole skill-specific tool surface (ADR-0036 D1). The model never
//! sees per-skill tools. Discovery is a `list_skills` call returning the catalog
//! as *data* (tier 1); activation is a `Skill { skill }` call whose result is the
//! instruction body, injected into the transcript as an ordinary tool result
//! (tier 2). References and scripts (tier 3) and authoring are done with the
//! built-in `read` / `bash` / `write` tools over the skill's materialized files —
//! there is no dedicated tool for them. The catalog is deliberately kept out of
//! the tool descriptors so a changing skill set never perturbs a pinned surface
//! (ADR-0036 D2/D7).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::state::Store;
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput, invoke_raw_tool};

use crate::registry::SkillRegistry;
use crate::spec::{SkillContext, SkillSpec, truncate_chars};

/// A shared, live record of file paths the run has touched, so conditional
/// (`paths`) skills surface once a matching file is accessed (ADR-0036: `paths`).
/// The host records paths (e.g. from a gate observing `read`/`write`/`edit`); the
/// `list_skills` tool reads them. Cheap to clone (shared handle).
#[derive(Clone, Default)]
pub struct PathActivations {
    touched: Arc<Mutex<BTreeSet<String>>>,
}

impl PathActivations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a touched path.
    pub fn record(&self, path: impl Into<String>) {
        if let Ok(mut set) = self.touched.lock() {
            set.insert(path.into());
        }
    }

    /// The paths touched so far.
    pub fn touched(&self) -> Vec<String> {
        self.touched
            .lock()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// Whether a conditional skill is surfaced: unconditional skills always are; a
/// skill with `paths` surfaces once one of its globs matches a touched path.
fn is_surfaced(skill: &SkillSpec, activations: Option<&PathActivations>) -> bool {
    if skill.paths.is_empty() {
        return true;
    }
    let Some(activations) = activations else {
        return false;
    };
    let touched = activations.touched();
    skill.paths.iter().any(|pattern| {
        glob::Pattern::new(pattern)
            .map(|p| touched.iter().any(|path| p.matches(path)))
            .unwrap_or(false)
    })
}

/// A gate that observes the `path`/`pattern` arguments of tool calls, recording
/// them into [`PathActivations`] so conditional (`paths`) skills surface, then
/// delegates the decision to `inner`. Observation only — it never changes the
/// decision. The composition root wraps its base gate with this.
pub struct RecordingGate {
    inner: Arc<dyn ToolGateHook>,
    activations: PathActivations,
}

/// Session-local skill capability restrictions. Each activated skill with a
/// non-empty `allowed_tools` list adds one conjunctive layer, so activation can
/// only narrow the host/platform gate and can never restore authority removed
/// by an earlier layer.
#[derive(Clone, Default)]
pub struct ActiveSkillTools {
    layers: Arc<Mutex<Vec<BTreeSet<String>>>>,
}

impl ActiveSkillTools {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn narrow(&self, patterns: &[String]) {
        if patterns.is_empty() {
            return;
        }
        if let Ok(mut layers) = self.layers.lock() {
            layers.push(patterns.iter().cloned().collect());
        }
    }

    pub fn allows(&self, tool_id: &str) -> bool {
        self.layers.lock().is_ok_and(|layers| {
            layers.iter().all(|layer| {
                layer.iter().any(|pattern| {
                    glob::Pattern::new(pattern)
                        .map(|pattern| pattern.matches(tool_id))
                        .unwrap_or(false)
                })
            })
        })
    }
}

/// Post-platform skill gate. The inner gate always decides first; only an
/// `Allow` is eligible for skill narrowing. Skill discovery/activation remain
/// reachable, but each new activation is conjunctive and therefore cannot be
/// used to widen the accumulated restriction.
pub struct SkillAllowedToolsGate {
    inner: Arc<dyn ToolGateHook>,
    active: ActiveSkillTools,
}

impl SkillAllowedToolsGate {
    pub fn new(inner: Arc<dyn ToolGateHook>, active: ActiveSkillTools) -> Self {
        Self { inner, active }
    }
}

#[async_trait]
impl ToolGateHook for SkillAllowedToolsGate {
    async fn gate(&self, call: &ToolCall, state: &Store) -> GateOutcome {
        let platform = self.inner.gate(call, state).await;
        if platform != GateOutcome::Allow {
            return platform;
        }
        if matches!(call.tool_id.as_str(), SKILL_LIST_TOOL_ID | SKILL_TOOL_ID)
            || self.active.allows(&call.tool_id)
        {
            GateOutcome::Allow
        } else {
            GateOutcome::Block {
                reason: format!(
                    "tool `{}` is outside the active skill allowed_tools intersection",
                    call.tool_id
                ),
            }
        }
    }
}

impl RecordingGate {
    pub fn new(inner: Arc<dyn ToolGateHook>, activations: PathActivations) -> Self {
        Self { inner, activations }
    }
}

#[async_trait]
impl ToolGateHook for RecordingGate {
    async fn gate(&self, ctx: &ToolCall, state: &Store) -> GateOutcome {
        for key in ["path", "pattern"] {
            if let Some(path) = ctx.arguments.get(key).and_then(|v| v.as_str()) {
                self.activations.record(path);
            }
        }
        self.inner.gate(ctx, state).await
    }
}

/// Expand a leading `/skill-name [args]` in a user message into the skill's
/// resolved instructions (ADR-0036 user invocation), honoring `user_invocable`.
/// Non-user messages, non-slash text, unknown skills, and non-user-invocable
/// skills pass through unchanged.
pub fn expand_slash_commands(
    registry: &dyn SkillRegistry,
    session_id: &str,
    input: Vec<Message>,
) -> Vec<Message> {
    input
        .into_iter()
        .map(|message| {
            if message.role != Role::User {
                return message;
            }
            let text: String = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let Some(rest) = text.trim_start().strip_prefix('/') else {
                return message;
            };
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            match registry.get(name.trim()) {
                Some(skill) if skill.user_invocable => Message::text(
                    message.id,
                    Role::User,
                    render_user_invocation(&skill, args.trim(), Some(session_id)),
                ),
                _ => message,
            }
        })
        .collect()
}

/// Per-entry cap on the catalog description/when-to-use, so a large skill set
/// keeps `list_skills` output bounded (ADR-0036: size limits).
const CATALOG_FIELD_CAP: usize = 200;

/// The single, stable id of the skill-activation tool.
pub const SKILL_TOOL_ID: &str = "Skill";
/// The id of the skill-discovery tool.
pub const SKILL_LIST_TOOL_ID: &str = "list_skills";

const SKILL_TOOL_SUMMARY: &str = "Activate a skill: inject its instructions into the conversation.\n\nCall `list_skills` first to see what is available, then activate one by id BEFORE doing the work; the skill's instructions are returned as the result. Skills provide specialized, repository-specific procedures and domain knowledge.";

const LIST_TOOL_SUMMARY: &str = "List the skills available to activate (id, description, when-to-use). Returns metadata only — call `Skill { skill }` to load a skill's full instructions. Use the optional `query` to filter by substring.";

/// The stable descriptor for the `Skill` activation tool. It carries no catalog
/// (that is served by `list_skills`), so its hash does not move when the skill set
/// changes (ADR-0036 D2/D7).
pub fn skill_tool_descriptor() -> ToolDescriptor {
    ToolDescriptor::pinned(
        "skills",
        SKILL_TOOL_ID,
        SKILL_TOOL_SUMMARY,
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The id of the skill to activate (from `list_skills`)."
                },
                "args": {
                    "type": "string",
                    "description": "Optional free-text arguments passed to the skill."
                }
            },
            "required": ["skill"]
        }),
    )
}

/// The stable descriptor for the `list_skills` discovery tool.
pub fn list_skills_tool_descriptor() -> ToolDescriptor {
    ToolDescriptor::pinned(
        "skills",
        SKILL_LIST_TOOL_ID,
        LIST_TOOL_SUMMARY,
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional case-insensitive substring filter over id/name/description."
                }
            }
        }),
    )
}

/// One catalog entry as data: model-facing identity plus provenance. Never the
/// body — that is tier-2, loaded on activation.
fn catalog_entry(skill: &SkillSpec) -> serde_json::Value {
    serde_json::json!({
        "id": skill.id,
        "name": skill.name,
        "description": truncate_chars(&skill.description, CATALOG_FIELD_CAP),
        "when_to_use": skill
            .when_to_use
            .as_deref()
            .map(|w| truncate_chars(w, CATALOG_FIELD_CAP)),
        "argument_hint": skill.argument_hint,
        "category": skill.category,
        "provenance": skill.provenance,
    })
}

fn matches_query(skill: &SkillSpec, query: &str) -> bool {
    let hay = format!(
        "{} {} {} {}",
        skill.id,
        skill.name,
        skill.description,
        skill.when_to_use.as_deref().unwrap_or("")
    )
    .to_lowercase();
    hay.contains(query)
}

/// Substitute `$ARGUMENTS` (the whole arg string) and `$1`..`$9` (whitespace-split
/// positionals) in a skill body. Returns the rewritten text and whether any token
/// was substituted, so the caller can decide whether to echo the raw args. An
/// out-of-range positional expands to empty; `$0`/`$<non-digit>` is left as-is.
fn substitute_arguments(body: &str, args: &str) -> (String, bool) {
    let positional: Vec<&str> = args.split_whitespace().collect();
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut used = false;
    let mut i = 0;
    while i < body.len() {
        if body[i..].starts_with("$ARGUMENTS") {
            out.push_str(args.trim());
            used = true;
            i += "$ARGUMENTS".len();
        } else if bytes[i] == b'$'
            && i + 1 < body.len()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 1] != b'0'
        {
            let idx = (bytes[i + 1] - b'0') as usize;
            out.push_str(positional.get(idx - 1).copied().unwrap_or(""));
            used = true;
            i += 2;
        } else {
            let ch = body[i..].chars().next().expect("char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    (out, used)
}

/// Replace `${SKILL_DIR}` and `${SESSION_ID}` with their concrete values. An
/// unavailable value leaves its token in place (so the author can spot it), which
/// matches Hermes' behavior.
fn substitute_template(body: &str, skill_dir: Option<&str>, session_id: Option<&str>) -> String {
    let mut out = body.to_string();
    if let Some(dir) = skill_dir {
        out = out.replace("${SKILL_DIR}", dir);
    }
    if let Some(session) = session_id {
        out = out.replace("${SESSION_ID}", session);
    }
    out
}

/// The skill's instructions with `${SKILL_DIR}`/`${SESSION_ID}` and argument
/// tokens substituted; the `bool` is whether an argument token was used.
fn resolved_body(skill: &SkillSpec, args: &str, session_id: Option<&str>) -> (String, bool) {
    let templated = substitute_template(&skill.body, skill.dir.as_deref(), session_id);
    substitute_arguments(&templated, args)
}

/// The text a user `/name args` invocation injects into the conversation: the
/// skill's resolved instructions (no tool-result header). The caller checks
/// `user_invocable` before calling this.
pub fn render_user_invocation(skill: &SkillSpec, args: &str, session_id: Option<&str>) -> String {
    resolved_body(skill, args, session_id).0
}

/// The inline activation result: a header naming the skill, its resolved
/// instructions, and — only when the body used no argument token — the raw args
/// echoed for the model.
fn render_activation(skill: &SkillSpec, args: &str, session_id: Option<&str>) -> String {
    let (body, used) = resolved_body(skill, args, session_id);
    let mut out = format!("Skill: {}\n\n{}", skill.name, body);
    let args = args.trim();
    if !args.is_empty() && !used {
        out.push_str("\n\nArguments: ");
        out.push_str(args);
    }
    out
}

/// Discovery tool (tier 1): returns the activatable-skill catalog as JSON data.
/// Only model-invocable skills are listed; an optional `query` filters them.
pub struct ListSkillsTool {
    registry: Arc<dyn SkillRegistry>,
    activations: Option<PathActivations>,
}

impl ListSkillsTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self {
            registry,
            activations: None,
        }
    }

    /// Wire the touched-path record so conditional (`paths`) skills surface once a
    /// matching file is accessed. Without it, conditional skills stay hidden.
    #[must_use]
    pub fn with_path_activations(mut self, activations: PathActivations) -> Self {
        self.activations = Some(activations);
        self
    }

    pub fn descriptor(&self) -> ToolDescriptor {
        list_skills_tool_descriptor()
    }
}

impl std::fmt::Debug for ListSkillsTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListSkillsTool").finish_non_exhaustive()
    }
}

#[async_trait]
impl RawTool for ListSkillsTool {
    fn id(&self) -> &str {
        SKILL_LIST_TOOL_ID
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let query = call
            .arguments
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| q.trim().to_lowercase())
            .filter(|q| !q.is_empty());
        let entries: Vec<serde_json::Value> = self
            .registry
            .list()
            .iter()
            .filter(|s| s.model_invocable)
            .filter(|s| is_surfaced(s, self.activations.as_ref()))
            .filter(|s| query.as_deref().is_none_or(|q| matches_query(s, q)))
            .map(catalog_entry)
            .collect();
        let payload = serde_json::json!({
            "skills": entries,
            "hint": "Activate one with Skill { skill: \"<id>\" }.",
        });
        Ok(ToolOutput::ok(call.call_id, payload.to_string()))
    }
}

/// Activation tool (tier 2), resolving against a [`SkillRegistry`]. Constructed
/// per session so it can resolve `${SESSION_ID}` without the kernel threading a
/// session id through the tool-call boundary.
pub struct SkillTool {
    registry: Arc<dyn SkillRegistry>,
    session_id: Option<String>,
    agent_tool: Option<Arc<dyn RawTool>>,
    active_tools: Option<ActiveSkillTools>,
}

impl SkillTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self {
            registry,
            session_id: None,
            agent_tool: None,
            active_tools: None,
        }
    }

    /// Bind the session id used to resolve `${SESSION_ID}` in activated bodies.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Wire the runner used for `context: fork` skills. Without it, a fork skill
    /// falls back to inline activation.
    #[must_use]
    pub fn with_agent_tool(mut self, tool: Arc<dyn RawTool>) -> Self {
        self.agent_tool = Some(tool);
        self
    }

    #[must_use]
    pub fn with_active_tools(mut self, active_tools: ActiveSkillTools) -> Self {
        self.active_tools = Some(active_tools);
        self
    }

    /// The stable, catalog-free descriptor. Convenience for the composition root.
    pub fn descriptor(&self) -> ToolDescriptor {
        skill_tool_descriptor()
    }
}

impl std::fmt::Debug for SkillTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillTool").finish_non_exhaustive()
    }
}

#[async_trait]
impl RawTool for SkillTool {
    fn id(&self) -> &str {
        SKILL_TOOL_ID
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let name = call
            .arguments
            .get("skill")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(name) = name else {
            return Ok(ToolOutput::error(
                call.call_id,
                "the `skill` argument is required",
            ));
        };
        // A leading slash is a user-invocation affordance; accept it here too.
        let key = name.trim_start_matches('/');
        let Some(skill) = self.registry.get(key) else {
            return Ok(ToolOutput::error(
                call.call_id,
                format!("unknown skill: {name}"),
            ));
        };
        if !skill.model_invocable {
            return Ok(ToolOutput::error(
                call.call_id,
                format!("skill `{key}` cannot be activated by the model"),
            ));
        }
        let args = call
            .arguments
            .get("args")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let session = self.session_id.as_deref();
        if let Some(active_tools) = &self.active_tools {
            active_tools.narrow(&skill.allowed_tools);
        }

        // A `context: fork` skill runs as a sub-agent (when a runner is wired),
        // returning its reply as the tool result; otherwise it falls back to inline.
        if skill.context == SkillContext::Fork
            && let Some(agent_tool) = &self.agent_tool
        {
            let (prompt, _) = resolved_body(&skill, args, session);
            let result = invoke_raw_tool(
                agent_tool.as_ref(),
                ToolCall {
                    call_id: format!("skill-fork-{}", skill.id),
                    tool_id: agent_tool.id().to_string(),
                    arguments: serde_json::json!({
                        "agent_id": skill.id,
                        "seed": vec![Message::text(
                            MessageId(format!("skill-fork-{}", skill.id)),
                            Role::User,
                            prompt,
                        )],
                    }),
                },
                None,
            )
            .await;
            return Ok(match result {
                Ok(output) if !output.is_error => ToolOutput::ok(call.call_id, output.content),
                Ok(output) => ToolOutput::error(call.call_id, output.content),
                Err(err) => ToolOutput::error(call.call_id, format!("skill fork failed: {err}")),
            });
        }
        Ok(ToolOutput::ok(
            call.call_id,
            render_activation(&skill, args, session),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::InMemorySkillRegistry;
    use crate::spec::SkillProvenance;

    fn registry() -> Arc<InMemorySkillRegistry> {
        Arc::new(InMemorySkillRegistry::from_specs([
            SkillSpec::new(
                "commit",
                "Commit",
                "Make a git commit",
                "SECRET-STEP: sign it",
            )
            .with_when_to_use("recording changes"),
            SkillSpec {
                model_invocable: false,
                ..SkillSpec::new("secret", "Secret", "hidden", "body")
            },
        ]))
    }

    fn call(id: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: "c1".into(),
            tool_id: id.into(),
            arguments: args,
        }
    }

    #[test]
    fn skill_descriptor_is_stable_and_catalog_free() {
        // The activation descriptor never carries the catalog, so a changing skill
        // set cannot perturb its hashed surface (ADR-0036 D2/D7).
        let a = skill_tool_descriptor();
        assert_eq!(a.id, SKILL_TOOL_ID);
        assert!(!a.description.contains("commit"));
        assert!(a.description.contains("list_skills"));
        // Identical regardless of any registry state.
        assert_eq!(a.content_hash, skill_tool_descriptor().content_hash);
    }

    #[tokio::test]
    async fn list_skills_returns_metadata_excluding_hidden_and_body() {
        let tool = ListSkillsTool::new(registry());
        let out = tool
            .invoke(call(SKILL_LIST_TOOL_ID, serde_json::json!({})))
            .await
            .unwrap();
        assert!(!out.is_error);
        let v: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        let skills = v["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 1, "hidden skill excluded");
        assert_eq!(skills[0]["id"], "commit");
        assert_eq!(skills[0]["when_to_use"], "recording changes");
        assert_eq!(skills[0]["provenance"], "delivered");
        // tier-1 is metadata only — the body must not appear in discovery.
        assert!(!out.content.contains("SECRET-STEP"));
    }

    #[tokio::test]
    async fn list_catalog_entries_are_length_bounded() {
        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "big",
            "Big",
            "z".repeat(1000),
            "body",
        )]));
        let out = ListSkillsTool::new(registry)
            .invoke(call(SKILL_LIST_TOOL_ID, serde_json::json!({})))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        let desc = v["skills"][0]["description"].as_str().unwrap();
        assert!(
            desc.chars().count() <= CATALOG_FIELD_CAP,
            "catalog entry bounded"
        );
        assert!(desc.ends_with('…'));
    }

    #[tokio::test]
    async fn conditional_skill_surfaces_only_after_a_matching_path_is_touched() {
        let registry = Arc::new(InMemorySkillRegistry::from_specs([
            SkillSpec::new("always", "Always", "unconditional", "b"),
            SkillSpec::new("rusty", "Rusty", "for rust files", "b")
                .with_paths(vec!["src/**/*.rs".into()]),
        ]));
        let activations = PathActivations::new();
        let tool = ListSkillsTool::new(registry).with_path_activations(activations.clone());

        // Before touching a matching file: only the unconditional skill.
        let before = tool
            .invoke(call(SKILL_LIST_TOOL_ID, serde_json::json!({})))
            .await
            .unwrap();
        assert!(before.content.contains("always"));
        assert!(
            !before.content.contains("rusty"),
            "conditional hidden: {}",
            before.content
        );

        // Touch a non-matching then a matching path.
        activations.record("docs/readme.md");
        activations.record("src/app/main.rs");

        let after = tool
            .invoke(call(SKILL_LIST_TOOL_ID, serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            after.content.contains("rusty"),
            "surfaced after match: {}",
            after.content
        );
    }

    #[tokio::test]
    async fn conditional_skill_stays_hidden_without_activations_wired() {
        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "rusty", "Rusty", "d", "b",
        )
        .with_paths(vec!["*.rs".into()])]));
        let out = ListSkillsTool::new(registry)
            .invoke(call(SKILL_LIST_TOOL_ID, serde_json::json!({})))
            .await
            .unwrap();
        assert!(!out.content.contains("rusty"));
    }

    #[tokio::test]
    async fn list_skills_query_filters() {
        let tool = ListSkillsTool::new(registry());
        let hit = tool
            .invoke(call(
                SKILL_LIST_TOOL_ID,
                serde_json::json!({ "query": "COMMIT" }),
            ))
            .await
            .unwrap();
        assert!(hit.content.contains("commit"));
        let miss = tool
            .invoke(call(
                SKILL_LIST_TOOL_ID,
                serde_json::json!({ "query": "zzz" }),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&miss.content).unwrap();
        assert!(v["skills"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_skills_reports_agent_created_provenance() {
        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "draft",
            "Draft",
            "authored this run",
            "body",
        )
        .with_provenance(SkillProvenance::AgentCreated)]));
        let out = ListSkillsTool::new(registry)
            .invoke(call(SKILL_LIST_TOOL_ID, serde_json::json!({})))
            .await
            .unwrap();
        assert!(out.content.contains("agent_created"));
    }

    #[tokio::test]
    async fn activation_returns_the_body_with_args() {
        let tool = SkillTool::new(registry());
        let out = tool
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "commit", "args": "-m fix" }),
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("Skill: Commit"));
        assert!(out.content.contains("SECRET-STEP: sign it"));
        assert!(out.content.contains("Arguments: -m fix"));
    }

    #[tokio::test]
    async fn unknown_and_missing_and_hidden_are_model_visible_errors() {
        let tool = SkillTool::new(registry());
        assert!(
            tool.invoke(call(SKILL_TOOL_ID, serde_json::json!({ "skill": "nope" })))
                .await
                .unwrap()
                .is_error
        );
        assert!(
            tool.invoke(call(SKILL_TOOL_ID, serde_json::json!({})))
                .await
                .unwrap()
                .is_error
        );
        assert!(
            tool.invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "secret" })
            ))
            .await
            .unwrap()
            .is_error
        );
    }

    #[tokio::test]
    async fn substitutes_positional_and_all_arguments() {
        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "run",
            "Run",
            "d",
            "first=$1 rest=$ARGUMENTS missing=$2",
        )]));
        let out = SkillTool::new(registry)
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "run", "args": "a" }),
            ))
            .await
            .unwrap();
        assert!(out.content.contains("first=a"));
        assert!(out.content.contains("rest=a"));
        assert!(
            out.content.contains("missing="),
            "out-of-range positional is empty"
        );
        // the body used tokens, so no raw-args footer is appended.
        assert!(!out.content.contains("Arguments:"));
    }

    #[tokio::test]
    async fn dollar_zero_and_non_digit_tokens_are_left_literal() {
        // The substituter treats ONLY `$1`..`$9` and `$ARGUMENTS` as tokens. `$0`
        // (excluded by `bytes[i+1] != b'0'`) and `$<non-digit>` fall through
        // untouched — and since no real token fired, the raw args are echoed.
        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "cost",
            "Cost",
            "d",
            "price=$0 flag=$x tail=$",
        )]));
        let out = SkillTool::new(registry)
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "cost", "args": "hi there" }),
            ))
            .await
            .unwrap();
        assert!(out.content.contains("price=$0"), "{}", out.content);
        assert!(out.content.contains("flag=$x"), "{}", out.content);
        assert!(out.content.contains("tail=$"), "{}", out.content);
        // No positional/ARGUMENTS token was consumed, so the footer echoes the args.
        assert!(
            out.content.contains("Arguments: hi there"),
            "unused-token body still echoes raw args: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn whitespace_only_args_append_no_footer() {
        // A body with no token + whitespace-only args must NOT emit an empty
        // "Arguments:" footer (args are trimmed before the emptiness check).
        let out = SkillTool::new(registry())
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "commit", "args": "   " }),
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            !out.content.contains("Arguments:"),
            "whitespace-only args must not emit a footer: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn an_invalid_path_glob_never_panics_and_stays_hidden() {
        // A skill whose `paths` glob fails to compile must be swallowed (unwrap_or
        // false) — it stays hidden rather than panicking or fail-open surfacing.
        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "broken", "Broken", "bad glob", "b",
        )
        .with_paths(vec!["[".into()])]));
        let activations = PathActivations::new();
        activations.record("anything.rs");
        let out = ListSkillsTool::new(registry)
            .with_path_activations(activations)
            .invoke(call(SKILL_LIST_TOOL_ID, serde_json::json!({})))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            !out.content.contains("broken"),
            "an uncompilable glob stays hidden: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn substitutes_skill_dir_and_session_id() {
        let spec = SkillSpec {
            dir: Some("skills/deploy".into()),
            ..SkillSpec::new(
                "deploy",
                "Deploy",
                "d",
                "run ${SKILL_DIR}/x.sh in ${SESSION_ID}",
            )
        };
        let registry = Arc::new(InMemorySkillRegistry::from_specs([spec]));
        let out = SkillTool::new(registry)
            .with_session_id("sess-1")
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "deploy" }),
            ))
            .await
            .unwrap();
        assert!(
            out.content.contains("run skills/deploy/x.sh in sess-1"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn unresolved_template_token_is_left_in_place() {
        // No dir and no session id: tokens survive so the author can spot them.
        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "d",
            "D",
            "x",
            "dir=${SKILL_DIR} sess=${SESSION_ID}",
        )]));
        let out = SkillTool::new(registry)
            .invoke(call(SKILL_TOOL_ID, serde_json::json!({ "skill": "d" })))
            .await
            .unwrap();
        assert!(out.content.contains("dir=${SKILL_DIR}"));
        assert!(out.content.contains("sess=${SESSION_ID}"));
    }

    #[tokio::test]
    async fn echoes_raw_args_only_when_body_uses_no_token() {
        let out = SkillTool::new(registry())
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "commit", "args": "-m x" }),
            ))
            .await
            .unwrap();
        // `commit` body has no $-token, so the raw args are echoed.
        assert!(out.content.contains("Arguments: -m x"));
    }

    struct EchoRunner;
    #[async_trait]
    impl RawTool for EchoRunner {
        fn id(&self) -> &str {
            "test_agent"
        }
        async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
            let agent_id = call.arguments["agent_id"]
                .as_str()
                .ok_or_else(|| ToolError::InvalidArguments("agent_id is missing".into()))?;
            let seed: Vec<Message> = serde_json::from_value(call.arguments["seed"].clone())
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            let prompt = seed.first().map(|m| m.text_content()).unwrap_or_default();
            Ok(ToolOutput::ok(
                call.call_id,
                format!("forked[{agent_id}]: {prompt}"),
            ))
        }
    }

    #[tokio::test]
    async fn fork_skill_runs_through_the_runner() {
        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "review",
            "Review",
            "d",
            "do the review of $ARGUMENTS",
        )
        .with_context(SkillContext::Fork)]));
        let out = SkillTool::new(registry)
            .with_agent_tool(Arc::new(EchoRunner))
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "review", "args": "PR-7" }),
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        // The runner's reply is returned verbatim — not the inline "Skill:" header.
        assert_eq!(out.content, "forked[review]: do the review of PR-7");
    }

    #[tokio::test]
    async fn a_fork_skill_whose_runner_errors_surfaces_a_model_visible_error() {
        // The fork failure arm: when the sub-agent runner returns Err, the activation
        // becomes a model-visible error result (never a panic or a silent success).
        struct FailingRunner;
        #[async_trait]
        impl RawTool for FailingRunner {
            fn id(&self) -> &str {
                "test_agent"
            }
            async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
                Err(ToolError::Execution("runner boom".to_string()))
            }
        }

        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "review",
            "Review",
            "d",
            "review $ARGUMENTS",
        )
        .with_context(SkillContext::Fork)]));
        let out = SkillTool::new(registry)
            .with_agent_tool(Arc::new(FailingRunner))
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "review", "args": "PR-7" }),
            ))
            .await
            .unwrap();
        assert!(
            out.is_error,
            "a fork runner failure is a model-visible error"
        );
        assert!(out.content.contains("skill fork failed"));
    }

    #[tokio::test]
    async fn a_conditional_skill_hidden_from_the_catalog_is_still_activatable_by_id() {
        // `paths` is progressive DISCLOSURE (what `list_skills` surfaces), NOT
        // authorization — authorization is `model_invocable`. A model that names a
        // hidden-but-invocable skill's id activates it. Pinned so the disclosure-vs-authz
        // split stays explicit: to bar activation, make the skill non-model-invocable.
        let skill = SkillSpec::new("deploy", "Deploy", "d", "the deploy steps")
            .with_paths(vec!["**/Dockerfile".to_string()]);
        let registry = Arc::new(InMemorySkillRegistry::from_specs([skill]));

        // Discovery hides it (no matching path touched, no activations wired).
        let listed = ListSkillsTool::new(registry.clone())
            .invoke(call(SKILL_LIST_TOOL_ID, serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            !listed.content.contains("deploy"),
            "a conditional skill is hidden from the catalog until surfaced"
        );

        // Activation by id still works — `paths` gates disclosure, not activation.
        let out = SkillTool::new(registry)
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "deploy" }),
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("the deploy steps"));
    }

    #[tokio::test]
    async fn fork_skill_without_a_runner_falls_back_to_inline() {
        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "review",
            "Review",
            "d",
            "inline body",
        )
        .with_context(SkillContext::Fork)]));
        let out = SkillTool::new(registry)
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "review" }),
            ))
            .await
            .unwrap();
        assert!(out.content.contains("Skill: Review"));
        assert!(out.content.contains("inline body"));
    }

    struct AllowGate;
    #[async_trait]
    impl ToolGateHook for AllowGate {
        async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateOutcome {
            GateOutcome::Allow
        }
    }

    fn ctx(tool_id: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            tool_id: tool_id.into(),
            call_id: "c1".into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn recording_gate_records_paths_and_delegates() {
        let activations = PathActivations::new();
        let gate = RecordingGate::new(Arc::new(AllowGate), activations.clone());
        let state = Store::new();
        assert_eq!(
            gate.gate(
                &ctx("read", serde_json::json!({ "path": "src/main.rs" })),
                &state
            )
            .await,
            GateOutcome::Allow
        );
        gate.gate(
            &ctx("glob", serde_json::json!({ "pattern": "*.rs" })),
            &state,
        )
        .await;
        gate.gate(&ctx("bash", serde_json::json!({ "command": "ls" })), &state)
            .await;
        let touched = activations.touched();
        assert!(touched.contains(&"src/main.rs".to_string()));
        assert!(touched.contains(&"*.rs".to_string()));
        assert_eq!(
            touched.len(),
            2,
            "only path/pattern args recorded: {touched:?}"
        );
    }

    #[tokio::test]
    async fn skill_allowed_tools_only_narrows_and_is_monotonic() {
        let active = ActiveSkillTools::new();
        let gate = SkillAllowedToolsGate::new(Arc::new(AllowGate), active.clone());
        let state = Store::new();

        assert_eq!(
            gate.gate(&ctx("bash", serde_json::json!({})), &state).await,
            GateOutcome::Allow
        );
        active.narrow(&["read".into(), "mcp__github__*".into()]);
        assert_eq!(
            gate.gate(&ctx("read", serde_json::json!({})), &state).await,
            GateOutcome::Allow
        );
        assert!(matches!(
            gate.gate(&ctx("bash", serde_json::json!({})), &state).await,
            GateOutcome::Block { .. }
        ));
        active.narrow(&["read".into(), "bash".into()]);
        assert!(matches!(
            gate.gate(&ctx("bash", serde_json::json!({})), &state).await,
            GateOutcome::Block { .. }
        ));
        assert_eq!(
            gate.gate(&ctx(SKILL_TOOL_ID, serde_json::json!({})), &state)
                .await,
            GateOutcome::Allow
        );
    }

    #[tokio::test]
    async fn skill_gate_never_overrides_a_platform_denial() {
        struct DenyGate;
        #[async_trait]
        impl ToolGateHook for DenyGate {
            async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateOutcome {
                GateOutcome::Block {
                    reason: "platform policy".into(),
                }
            }
        }

        let gate = SkillAllowedToolsGate::new(Arc::new(DenyGate), ActiveSkillTools::new());
        assert_eq!(
            gate.gate(&ctx("read", serde_json::json!({})), &Store::new())
                .await,
            GateOutcome::Block {
                reason: "platform policy".into(),
            }
        );
    }

    fn user_msg(text: &str) -> Message {
        Message::text(
            awaken_agent_contract::agent::message::Id("m".into()),
            Role::User,
            text,
        )
    }

    fn msg_text(message: &Message) -> String {
        message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn slash_command_expands_only_user_invocable_skills() {
        let registry = InMemorySkillRegistry::from_specs([
            SkillSpec::new("deploy", "Deploy", "d", "checklist for $ARGUMENTS"),
            SkillSpec {
                user_invocable: false,
                ..SkillSpec::new("secret", "Secret", "d", "hidden")
            },
        ]);
        // user-invocable → expands to the resolved body
        let out = expand_slash_commands(&registry, "sess", vec![user_msg("/deploy prod")]);
        assert_eq!(msg_text(&out[0]), "checklist for prod");
        // non-user-invocable / unknown / plain → unchanged
        let out = expand_slash_commands(
            &registry,
            "sess",
            vec![user_msg("/secret"), user_msg("/nope"), user_msg("hi")],
        );
        assert_eq!(msg_text(&out[0]), "/secret");
        assert_eq!(msg_text(&out[1]), "/nope");
        assert_eq!(msg_text(&out[2]), "hi");
    }

    #[test]
    fn slash_command_leaves_non_user_messages_untouched() {
        // The `role != User` early return: an assistant message that happens to
        // start with `/deploy` must NOT be expanded (only user turns invoke skills).
        let registry = InMemorySkillRegistry::from_specs([SkillSpec::new(
            "deploy",
            "Deploy",
            "d",
            "checklist for $ARGUMENTS",
        )]);
        let assistant = Message::text(
            awaken_agent_contract::agent::message::Id("a".into()),
            Role::Assistant,
            "/deploy prod",
        );
        let out = expand_slash_commands(&registry, "sess", vec![assistant]);
        assert_eq!(
            msg_text(&out[0]),
            "/deploy prod",
            "assistant message is passed through verbatim"
        );
    }

    #[tokio::test]
    async fn empty_and_whitespace_query_lists_all_skills() {
        // An empty/whitespace `query` is filtered to `None`, so it must not filter
        // anything out — the full model-invocable catalog is returned.
        let tool = ListSkillsTool::new(registry());
        for q in ["", "   "] {
            let out = tool
                .invoke(call(SKILL_LIST_TOOL_ID, serde_json::json!({ "query": q })))
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&out.content).unwrap();
            assert_eq!(
                v["skills"].as_array().unwrap().len(),
                1,
                "query {q:?} lists all invocable skills"
            );
        }
    }

    #[tokio::test]
    async fn list_skills_query_matches_when_to_use_field() {
        // matches_query folds in `when_to_use`; a query that hits only that field
        // must still surface the skill (id/name/description don't contain it).
        let tool = ListSkillsTool::new(registry());
        let out = tool
            .invoke(call(
                SKILL_LIST_TOOL_ID,
                serde_json::json!({ "query": "recording" }),
            ))
            .await
            .unwrap();
        assert!(
            out.content.contains("commit"),
            "when_to_use match surfaces the skill: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn leading_slash_is_accepted() {
        let out = SkillTool::new(registry())
            .invoke(call(
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "/commit" }),
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
    }
}
