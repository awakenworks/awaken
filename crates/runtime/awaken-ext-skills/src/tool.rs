//! The single `Skill` tool and its catalog-bearing descriptor.
//!
//! There is exactly one model-facing skill tool, id [`SKILL_TOOL_ID`]. The model
//! never sees per-skill tools: the *catalog* of activatable skills lives in the
//! tool's description (progressive disclosure — name + description + when-to-use),
//! and *activation* is a call `Skill { skill, args? }` whose result is the skill's
//! instruction body, injected into the transcript as an ordinary tool result. The
//! kernel sees one tool and a tool result; it never learns the concept "skill".

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};

use crate::registry::SkillRegistry;
use crate::spec::SkillSpec;

/// The single, stable id of the skill-activation tool.
pub const SKILL_TOOL_ID: &str = "Skill";

/// Cap on one catalog entry's rendered length, so a large skill set cannot blow
/// out the tool description.
const CATALOG_ENTRY_CAP: usize = 250;

const SKILL_TOOL_SUMMARY: &str = "Activate a skill: inject its instructions into the conversation.\n\nWhen a user's request matches an available skill, call this tool with the skill id BEFORE doing the work; the skill's instructions are returned as the result. Skills provide specialized, repository-specific procedures and domain knowledge.";

/// The model-visible descriptor for the `Skill` tool, with the activatable-skill
/// catalog rendered into its description. Only model-invocable skills are listed.
pub fn skill_tool_descriptor(registry: &dyn SkillRegistry) -> ToolDescriptor {
    let description = format!(
        "{SKILL_TOOL_SUMMARY}\n\n{}",
        render_catalog(&registry.list())
    );
    ToolDescriptor::pinned(
        "skills",
        SKILL_TOOL_ID,
        description,
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The id of the skill to activate (see the list above)."
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

/// Render the catalog block shown in the tool description: one line per
/// model-invocable skill, `- <id>: <description>[ — When to use: <when>]`,
/// truncated per entry. An empty set states so explicitly.
fn render_catalog(skills: &[SkillSpec]) -> String {
    let entries: Vec<String> = skills
        .iter()
        .filter(|s| s.model_invocable)
        .map(render_catalog_entry)
        .collect();
    if entries.is_empty() {
        return "Available skills: (none)".to_string();
    }
    format!("Available skills:\n{}", entries.join("\n"))
}

fn render_catalog_entry(skill: &SkillSpec) -> String {
    let mut line = format!("- {}: {}", skill.id, skill.description);
    if let Some(when) = &skill.when_to_use {
        line.push_str(" — When to use: ");
        line.push_str(when);
    }
    truncate(&line, CATALOG_ENTRY_CAP)
}

/// Truncate on a char boundary, appending an ellipsis when cut.
fn truncate(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut out: String = text.chars().take(cap.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The activation result: a header naming the skill, the instruction body, and
/// any caller arguments echoed for the model to act on.
fn render_activation(skill: &SkillSpec, args: &str) -> String {
    let mut out = format!("Skill: {}\n\n{}", skill.name, skill.body);
    let args = args.trim();
    if !args.is_empty() {
        out.push_str("\n\nArguments: ");
        out.push_str(args);
    }
    out
}

/// The single skill-activation tool, resolving against a [`SkillRegistry`].
pub struct SkillTool {
    registry: Arc<dyn SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self { registry }
    }

    /// The tool plus its catalog-bearing descriptor, ready to register and
    /// advertise. Convenience for the composition root.
    pub fn descriptor(&self) -> ToolDescriptor {
        skill_tool_descriptor(self.registry.as_ref())
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

    fn registry() -> Arc<InMemorySkillRegistry> {
        Arc::new(InMemorySkillRegistry::from_specs([
            SkillSpec::new("commit", "Commit", "Make a git commit", "Steps: ...")
                .with_when_to_use("recording changes"),
            SkillSpec {
                model_invocable: false,
                ..SkillSpec::new("secret", "Secret", "hidden", "body")
            },
        ]))
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: "c1".into(),
            tool_id: SKILL_TOOL_ID.into(),
            arguments: args,
        }
    }

    #[test]
    fn descriptor_lists_only_model_invocable_skills() {
        let desc = skill_tool_descriptor(registry().as_ref());
        assert_eq!(desc.id, SKILL_TOOL_ID);
        assert!(desc.description.contains("- commit: Make a git commit"));
        assert!(desc.description.contains("When to use: recording changes"));
        assert!(!desc.description.contains("secret"));
    }

    #[tokio::test]
    async fn activation_returns_the_body_with_args() {
        let tool = SkillTool::new(registry());
        let out = tool
            .invoke(call(
                serde_json::json!({ "skill": "commit", "args": "-m fix" }),
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("Skill: Commit"));
        assert!(out.content.contains("Steps: ..."));
        assert!(out.content.contains("Arguments: -m fix"));
    }

    #[tokio::test]
    async fn unknown_and_missing_and_hidden_are_model_visible_errors() {
        let tool = SkillTool::new(registry());
        let unknown = tool
            .invoke(call(serde_json::json!({ "skill": "nope" })))
            .await
            .unwrap();
        assert!(unknown.is_error);

        let missing = tool.invoke(call(serde_json::json!({}))).await.unwrap();
        assert!(missing.is_error);

        let hidden = tool
            .invoke(call(serde_json::json!({ "skill": "secret" })))
            .await
            .unwrap();
        assert!(hidden.is_error);
    }

    #[tokio::test]
    async fn leading_slash_is_accepted() {
        let tool = SkillTool::new(registry());
        let out = tool
            .invoke(call(serde_json::json!({ "skill": "/commit" })))
            .await
            .unwrap();
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn disclosure_is_progressive_body_only_on_activation() {
        // The descriptor (level-1 disclosure) advertises identity but never the
        // instruction body; the body (level-2) appears only in the activation
        // result. This is the whole point of progressive disclosure.
        let registry = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "deploy",
            "Deploy",
            "Ship a release",
            "SECRET-STEP: rotate the signing key first",
        )]));
        let desc = skill_tool_descriptor(registry.as_ref());
        assert!(desc.description.contains("- deploy: Ship a release"));
        assert!(
            !desc.description.contains("SECRET-STEP"),
            "the body must not leak into the catalog: {}",
            desc.description
        );

        let out = SkillTool::new(registry)
            .invoke(call(serde_json::json!({ "skill": "deploy" })))
            .await
            .unwrap();
        assert!(
            out.content.contains("SECRET-STEP"),
            "body loads on activation"
        );
    }

    #[test]
    fn long_catalog_entry_is_truncated() {
        let long = "x".repeat(400);
        let registry =
            InMemorySkillRegistry::from_specs([SkillSpec::new("big", "Big", long.clone(), "body")]);
        let desc = skill_tool_descriptor(&registry);
        assert!(
            desc.description.contains('…'),
            "an over-long entry is ellipsized"
        );
        assert!(
            !desc.description.contains(&long),
            "the full over-long description is not carried verbatim"
        );
    }

    #[test]
    fn empty_catalog_states_none() {
        let registry = InMemorySkillRegistry::from_specs([SkillSpec {
            model_invocable: false,
            ..SkillSpec::new("hidden", "Hidden", "nope", "body")
        }]);
        let desc = skill_tool_descriptor(&registry);
        assert!(desc.description.contains("Available skills: (none)"));
    }
}
