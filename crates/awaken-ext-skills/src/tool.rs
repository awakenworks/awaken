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

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};

use crate::registry::SkillRegistry;
use crate::spec::{SkillSpec, truncate_chars};

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

/// The activation result: a header naming the skill, its instructions (with
/// argument tokens substituted), and — only when the body used no token — the
/// raw args echoed for the model to act on.
fn render_activation(skill: &SkillSpec, args: &str) -> String {
    let (body, used) = substitute_arguments(&skill.body, args);
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
}

impl ListSkillsTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self { registry }
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

/// Activation tool (tier 2), resolving against a [`SkillRegistry`].
pub struct SkillTool {
    registry: Arc<dyn SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self { registry }
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
        Ok(ToolOutput::ok(
            call.call_id,
            render_activation(&skill, args),
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
