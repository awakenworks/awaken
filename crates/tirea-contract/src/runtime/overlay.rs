use crate::runtime::inference::context::{ContextWindowPolicy, InferenceOverride};
use crate::runtime::phase::{ActionSet, BeforeInferenceAction};
use serde::{Deserialize, Serialize};

/// Unified agent configuration overlay.
///
/// A partial agent configuration where every field is `Option` — `None`
/// means "use the base agent's default". Used by handoff, protocol layers,
/// and any mechanism that needs to apply a configuration delta on top of
/// a running agent.
///
/// # Decomposition
///
/// [`into_before_inference_actions`](AgentOverlay::into_before_inference_actions)
/// converts per-inference fields into the corresponding
/// [`BeforeInferenceAction`] variants. Run-level and scope fields are
/// **not** decomposed — they are consumed by other mechanisms (e.g.
/// `RunPolicy`, future `LifecycleAction`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentOverlay {
    // === Per-inference (decomposed into BeforeInferenceAction) ===
    /// Model selection and inference parameters (temperature, max_tokens,
    /// reasoning_effort, etc.). Emitted as `OverrideInference`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference: Option<InferenceOverride>,

    /// System prompt appended via `AddContextMessage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,

    /// Tool whitelist — emitted as `IncludeOnlyTools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,

    /// Tool blacklist — each entry emitted as `ExcludeTool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_tools: Option<Vec<String>>,

    // === Scope filtering (consumed by RunPolicy, not decomposed here) ===
    /// Skill whitelist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_skills: Option<Vec<String>>,

    /// Skill blacklist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_skills: Option<Vec<String>>,

    /// Agent delegation whitelist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_agents: Option<Vec<String>>,

    /// Agent delegation blacklist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_agents: Option<Vec<String>>,

    // === Run-level (future LifecycleAction, not decomposed here) ===
    /// Maximum tool-call rounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<usize>,

    /// Context window management policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<ContextWindowPolicy>,
}

impl AgentOverlay {
    /// Decompose per-inference fields into [`BeforeInferenceAction`] variants.
    ///
    /// Only fields that have corresponding actions are decomposed:
    /// - `inference` → `OverrideInference`
    /// - `system_prompt` → `AddContextMessage`
    /// - `excluded_tools` → `ExcludeTool` (one per entry)
    /// - `allowed_tools` → `IncludeOnlyTools`
    ///
    /// Scope fields (`allowed_skills`, `allowed_agents`, etc.) and run-level
    /// fields (`max_rounds`, `context_window`) are **not** emitted — they are
    /// consumed by other mechanisms.
    pub fn into_before_inference_actions(self) -> ActionSet<BeforeInferenceAction> {
        let mut actions = ActionSet::empty();

        if let Some(ovr) = self.inference {
            actions = actions.and(ActionSet::single(BeforeInferenceAction::OverrideInference(
                ovr,
            )));
        }

        if let Some(prompt) = self.system_prompt {
            actions = actions.and(ActionSet::single(BeforeInferenceAction::AddContextMessage(
                crate::runtime::inference::ContextMessage {
                    key: "handoff_prompt".into(),
                    content: prompt,
                    cooldown_turns: 0,
                    target: Default::default(),
                    consume_after_emit: false,
                },
            )));
        }

        if let Some(excluded) = self.excluded_tools {
            for tool_id in excluded {
                actions = actions.and(ActionSet::single(BeforeInferenceAction::ExcludeTool(
                    tool_id,
                )));
            }
        }

        if let Some(allowed) = self.allowed_tools {
            actions = actions.and(ActionSet::single(BeforeInferenceAction::IncludeOnlyTools(
                allowed,
            )));
        }

        actions
    }

    /// Merge `other` into `self` with last-wins semantics per field.
    pub fn merge(&mut self, other: AgentOverlay) {
        if other.inference.is_some() {
            match (&mut self.inference, other.inference) {
                (Some(existing), Some(incoming)) => existing.merge(incoming),
                (slot, incoming) => *slot = incoming,
            }
        }
        if other.system_prompt.is_some() {
            self.system_prompt = other.system_prompt;
        }
        if other.allowed_tools.is_some() {
            self.allowed_tools = other.allowed_tools;
        }
        if other.excluded_tools.is_some() {
            self.excluded_tools = other.excluded_tools;
        }
        if other.allowed_skills.is_some() {
            self.allowed_skills = other.allowed_skills;
        }
        if other.excluded_skills.is_some() {
            self.excluded_skills = other.excluded_skills;
        }
        if other.allowed_agents.is_some() {
            self.allowed_agents = other.allowed_agents;
        }
        if other.excluded_agents.is_some() {
            self.excluded_agents = other.excluded_agents;
        }
        if other.max_rounds.is_some() {
            self.max_rounds = other.max_rounds;
        }
        if other.context_window.is_some() {
            self.context_window = other.context_window;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::inference::context::{InferenceOverride, ReasoningEffort};

    #[test]
    fn empty_overlay_produces_no_actions() {
        let overlay = AgentOverlay::default();
        let actions = overlay.into_before_inference_actions();
        assert!(actions.is_empty());
    }

    #[test]
    fn decompose_inference_override() {
        let overlay = AgentOverlay {
            inference: Some(InferenceOverride {
                model: Some("claude-sonnet".into()),
                temperature: Some(0.7),
                ..Default::default()
            }),
            ..Default::default()
        };
        let actions: Vec<_> = overlay
            .into_before_inference_actions()
            .into_iter()
            .collect();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            BeforeInferenceAction::OverrideInference(ovr)
                if ovr.model.as_deref() == Some("claude-sonnet")
        ));
    }

    #[test]
    fn decompose_system_prompt() {
        let overlay = AgentOverlay {
            system_prompt: Some("You are a helpful assistant.".into()),
            ..Default::default()
        };
        let actions: Vec<_> = overlay
            .into_before_inference_actions()
            .into_iter()
            .collect();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            BeforeInferenceAction::AddContextMessage(ref cm) if cm.content.contains("helpful")
        ));
    }

    #[test]
    fn decompose_tool_filtering() {
        let overlay = AgentOverlay {
            excluded_tools: Some(vec!["Bash".into(), "Write".into()]),
            allowed_tools: Some(vec!["Read".into(), "Glob".into()]),
            ..Default::default()
        };
        let actions: Vec<_> = overlay
            .into_before_inference_actions()
            .into_iter()
            .collect();
        // 2 ExcludeTool + 1 IncludeOnlyTools = 3
        assert_eq!(actions.len(), 3);
        assert!(matches!(&actions[0], BeforeInferenceAction::ExcludeTool(id) if id == "Bash"));
        assert!(matches!(&actions[1], BeforeInferenceAction::ExcludeTool(id) if id == "Write"));
        assert!(
            matches!(&actions[2], BeforeInferenceAction::IncludeOnlyTools(ids) if ids.len() == 2)
        );
    }

    #[test]
    fn decompose_full_overlay() {
        let overlay = AgentOverlay {
            inference: Some(InferenceOverride {
                model: Some("gpt-4o".into()),
                ..Default::default()
            }),
            system_prompt: Some("Be concise.".into()),
            excluded_tools: Some(vec!["Bash".into()]),
            allowed_tools: Some(vec!["Read".into()]),
            // These should NOT produce actions
            allowed_skills: Some(vec!["commit".into()]),
            max_rounds: Some(5),
            ..Default::default()
        };
        let actions: Vec<_> = overlay
            .into_before_inference_actions()
            .into_iter()
            .collect();
        // OverrideInference + AddContextMessage + ExcludeTool + IncludeOnlyTools = 4
        assert_eq!(actions.len(), 4);
    }

    #[test]
    fn merge_last_wins() {
        let mut base = AgentOverlay {
            system_prompt: Some("base prompt".into()),
            allowed_tools: Some(vec!["Read".into()]),
            inference: Some(InferenceOverride {
                model: Some("model-a".into()),
                temperature: Some(0.5),
                ..Default::default()
            }),
            ..Default::default()
        };
        base.merge(AgentOverlay {
            system_prompt: Some("override prompt".into()),
            inference: Some(InferenceOverride {
                reasoning_effort: Some(ReasoningEffort::High),
                ..Default::default()
            }),
            max_rounds: Some(10),
            ..Default::default()
        });
        assert_eq!(base.system_prompt.as_deref(), Some("override prompt"));
        assert_eq!(base.allowed_tools, Some(vec!["Read".into()])); // not overwritten
        assert_eq!(base.max_rounds, Some(10));
        // inference merged: model preserved, reasoning added
        let inf = base.inference.unwrap();
        assert_eq!(inf.model.as_deref(), Some("model-a"));
        assert_eq!(inf.temperature, Some(0.5));
        assert_eq!(inf.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn merge_none_preserves_existing() {
        let mut base = AgentOverlay {
            system_prompt: Some("keep me".into()),
            max_rounds: Some(20),
            ..Default::default()
        };
        base.merge(AgentOverlay::default());
        assert_eq!(base.system_prompt.as_deref(), Some("keep me"));
        assert_eq!(base.max_rounds, Some(20));
    }

    #[test]
    fn serde_roundtrip() {
        let overlay = AgentOverlay {
            inference: Some(InferenceOverride {
                model: Some("claude-opus".into()),
                reasoning_effort: Some(ReasoningEffort::Budget(8000)),
                ..Default::default()
            }),
            system_prompt: Some("test prompt".into()),
            max_rounds: Some(15),
            ..Default::default()
        };
        let json = serde_json::to_string(&overlay).unwrap();
        let restored: AgentOverlay = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.inference.as_ref().unwrap().model.as_deref(),
            Some("claude-opus")
        );
        assert_eq!(restored.system_prompt.as_deref(), Some("test prompt"));
        assert_eq!(restored.max_rounds, Some(15));
    }

    #[test]
    fn serde_empty_fields_omitted() {
        let overlay = AgentOverlay {
            system_prompt: Some("only this".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&overlay).unwrap();
        assert!(!json.contains("inference"));
        assert!(!json.contains("max_rounds"));
        assert!(json.contains("system_prompt"));
    }
}
