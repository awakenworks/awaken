use super::actions::block_write_in_plan_mode;
use super::prompt::{approved_plan_hint, plan_mode_system_prompt};
use super::state::PlanModeState;
use async_trait::async_trait;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction, BeforeToolExecuteAction};

/// Stable plugin id for plan mode actions.
pub const PLAN_MODE_PLUGIN_ID: &str = "plan_mode";

/// Tools that are always allowed in plan mode (read-only exploration).
const READ_ONLY_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LSP",
    "WebSearch",
    "WebFetch",
    "AskUserQuestion",
    "TaskCreate",
    "TaskUpdate",
    "TaskGet",
    "TaskList",
    "TaskOutput",
    "TaskStop",
];

/// Tools that should be hidden from the model during plan mode.
const HIDDEN_IN_PLAN_MODE: &[&str] = &["EnterPlanMode", "EnterWorktree"];

/// Tools that should be hidden when NOT in plan mode.
const HIDDEN_OUTSIDE_PLAN_MODE: &[&str] = &["ExitPlanMode"];

/// Plan mode enforcement plugin.
///
/// Two-layer enforcement:
/// 1. **System prompt** (`before_inference`): instructs the model to only use read-only tools
/// 2. **Hard gate** (`before_tool_execute`): blocks write tools even if model ignores the prompt
pub struct PlanModePlugin;

impl PlanModePlugin {
    /// Create a new plan mode plugin.
    pub fn new() -> Self {
        Self
    }

    /// Check if a tool is always allowed in plan mode.
    fn is_read_only(tool_id: &str) -> bool {
        READ_ONLY_TOOLS.contains(&tool_id)
    }
}

impl Default for PlanModePlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentBehavior for PlanModePlugin {
    fn id(&self) -> &str {
        PLAN_MODE_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(PlanModeState);

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let state = ctx.snapshot_of::<PlanModeState>().ok().unwrap_or_default();

        let mut actions = ActionSet::empty();

        if state.active {
            // Inject plan mode system prompt
            actions = actions.and(ActionSet::single(BeforeInferenceAction::AddSystemContext(
                plan_mode_system_prompt(),
            )));

            // Hide tools that shouldn't be visible in plan mode
            for &tool_id in HIDDEN_IN_PLAN_MODE {
                actions = actions.and(ActionSet::single(BeforeInferenceAction::ExcludeTool(
                    tool_id.to_string(),
                )));
            }
        } else {
            // Hide ExitPlanMode when not in plan mode
            for &tool_id in HIDDEN_OUTSIDE_PLAN_MODE {
                actions = actions.and(ActionSet::single(BeforeInferenceAction::ExcludeTool(
                    tool_id.to_string(),
                )));
            }

            // If there's an approved plan, inject a lightweight hint
            if let Some(ref plan_ref) = state.approved_plan {
                actions = actions.and(ActionSet::single(BeforeInferenceAction::AddSessionContext(
                    approved_plan_hint(&plan_ref.summary),
                )));
            }
        }

        actions
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let state = ctx.snapshot_of::<PlanModeState>().ok().unwrap_or_default();

        if !state.active {
            return ActionSet::empty();
        }

        let Some(tool_id) = ctx.tool_name() else {
            return ActionSet::empty();
        };

        // ExitPlanMode is always allowed in plan mode
        if tool_id == "ExitPlanMode" {
            return ActionSet::empty();
        }

        // Read-only tools pass through
        if Self::is_read_only(tool_id) {
            return ActionSet::empty();
        }

        // Block everything else
        ActionSet::single(block_write_in_plan_mode(tool_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PlanRef;
    use serde_json::json;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::RunPolicy;
    use tirea_state::DocCell;

    fn plugin() -> PlanModePlugin {
        PlanModePlugin::new()
    }

    // -- before_inference tests --

    #[tokio::test]
    async fn inactive_hides_exit_plan_mode() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "plan_mode": { "active": false } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        let excludes: Vec<_> = actions
            .into_iter()
            .filter_map(|a| match a {
                BeforeInferenceAction::ExcludeTool(id) => Some(id),
                _ => None,
            })
            .collect();
        assert!(excludes.contains(&"ExitPlanMode".to_string()));
    }

    #[tokio::test]
    async fn active_injects_system_prompt() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "plan_mode": { "active": true } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        let has_system_context = actions
            .into_iter()
            .any(|a| matches!(a, BeforeInferenceAction::AddSystemContext(s) if s.contains("Plan mode is active")));
        assert!(has_system_context);
    }

    #[tokio::test]
    async fn active_hides_enter_plan_mode() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "plan_mode": { "active": true } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        let excludes: Vec<_> = actions
            .into_iter()
            .filter_map(|a| match a {
                BeforeInferenceAction::ExcludeTool(id) => Some(id),
                _ => None,
            })
            .collect();
        assert!(excludes.contains(&"EnterPlanMode".to_string()));
        assert!(!excludes.contains(&"ExitPlanMode".to_string()));
    }

    #[tokio::test]
    async fn no_state_hides_exit_plan_mode() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        let excludes: Vec<_> = actions
            .into_iter()
            .filter_map(|a| match a {
                BeforeInferenceAction::ExcludeTool(id) => Some(id),
                _ => None,
            })
            .collect();
        assert!(excludes.contains(&"ExitPlanMode".to_string()));
    }

    #[tokio::test]
    async fn approved_plan_injects_hint() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "plan_mode": {
                "active": false,
                "approved_plan": {
                    "thread_id": "t1",
                    "plan_id": "plan-1",
                    "summary": "Refactor auth module"
                }
            }
        }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        let has_hint = actions
            .into_iter()
            .any(|a| matches!(a, BeforeInferenceAction::AddSessionContext(s) if s.contains("Refactor auth module")));
        assert!(has_hint);
    }

    // -- before_tool_execute tests --

    #[tokio::test]
    async fn inactive_allows_all_tools() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "plan_mode": { "active": false } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("Bash", "call_1", None);

        let actions = AgentBehavior::before_tool_execute(&p, &ctx).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn active_allows_read_only_tools() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "plan_mode": { "active": true } }));

        for tool in &[
            "Read",
            "Glob",
            "Grep",
            "LSP",
            "WebSearch",
            "WebFetch",
            "AskUserQuestion",
        ] {
            let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
                .with_tool_info(tool, "call_1", None);
            let actions = AgentBehavior::before_tool_execute(&p, &ctx).await;
            assert!(
                actions.is_empty(),
                "Expected {tool} to be allowed in plan mode"
            );
        }
    }

    #[tokio::test]
    async fn active_blocks_write_tools() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "plan_mode": { "active": true } }));

        for tool in &["Bash", "Edit", "Write", "NotebookEdit"] {
            let args = json!({"file_path": "/some/other/file.rs"});
            let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
                .with_tool_info(tool, "call_1", Some(&args));
            let actions = AgentBehavior::before_tool_execute(&p, &ctx).await;
            assert!(
                !actions.is_empty(),
                "Expected {tool} to be blocked in plan mode"
            );
        }
    }

    #[tokio::test]
    async fn active_allows_exit_plan_mode() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({ "plan_mode": { "active": true } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("ExitPlanMode", "call_1", None);

        let actions = AgentBehavior::before_tool_execute(&p, &ctx).await;
        assert!(actions.is_empty());
    }

    // -- classification tests --

    #[test]
    fn read_only_tools_classified_correctly() {
        assert!(PlanModePlugin::is_read_only("Read"));
        assert!(PlanModePlugin::is_read_only("Glob"));
        assert!(PlanModePlugin::is_read_only("Grep"));
        assert!(!PlanModePlugin::is_read_only("Bash"));
        assert!(!PlanModePlugin::is_read_only("Write"));
        assert!(!PlanModePlugin::is_read_only("Edit"));
    }

    // -- state tests --

    #[test]
    fn plan_ref_roundtrip() {
        let pr = PlanRef {
            thread_id: "t1".into(),
            plan_id: "p1".into(),
            summary: "Refactor X".into(),
        };
        let json = serde_json::to_value(&pr).unwrap();
        let back: PlanRef = serde_json::from_value(json).unwrap();
        assert_eq!(pr, back);
    }
}
