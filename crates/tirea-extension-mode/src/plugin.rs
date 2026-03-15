use super::state::AgentModeState;
use async_trait::async_trait;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction};
use tirea_contract::runtime::run::TerminationReason;

/// Stable plugin id for mode switching behavior.
pub const MODE_PLUGIN_ID: &str = "agent_mode";

/// Mode switching enforcement plugin.
///
/// Detects pending mode switch requests in `before_inference` and terminates
/// the run with `Stopped("mode_switch")` so the `AgentOs::run()` auto-restart
/// loop can re-resolve with the new mode overlay.
pub struct ModePlugin;

impl ModePlugin {
    /// Create a new mode plugin.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ModePlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Stop code used internally to signal a mode switch.
pub const MODE_SWITCH_STOP_CODE: &str = "mode_switch";

#[async_trait]
impl AgentBehavior for ModePlugin {
    fn id(&self) -> &str {
        MODE_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(AgentModeState);

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let state = ctx.snapshot_of::<AgentModeState>().ok().unwrap_or_default();

        // If a mode switch was requested and differs from the active mode,
        // terminate so the outer loop can re-resolve.
        if let Some(ref requested) = state.requested_mode {
            if state.active_mode.as_ref() != Some(requested) {
                return ActionSet::single(BeforeInferenceAction::Terminate(
                    TerminationReason::stopped(MODE_SWITCH_STOP_CODE),
                ));
            }
        }

        ActionSet::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::RunPolicy;
    use tirea_state::DocCell;

    fn plugin() -> ModePlugin {
        ModePlugin::new()
    }

    #[tokio::test]
    async fn no_pending_switch_returns_empty() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn same_mode_returns_empty() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "agent_mode": {
                "active_mode": "fast",
                "requested_mode": "fast"
            }
        }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn pending_switch_terminates() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "agent_mode": {
                "active_mode": null,
                "requested_mode": "fast"
            }
        }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        assert_eq!(actions.len(), 1);
        let has_terminate = actions.into_iter().any(|a| {
            matches!(
                a,
                BeforeInferenceAction::Terminate(TerminationReason::Stopped(ref s))
                    if s.code == MODE_SWITCH_STOP_CODE
            )
        });
        assert!(has_terminate);
    }

    #[tokio::test]
    async fn different_mode_switch_terminates() {
        let p = plugin();
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "agent_mode": {
                "active_mode": "default",
                "requested_mode": "fast"
            }
        }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = AgentBehavior::before_inference(&p, &ctx).await;
        assert!(!actions.is_empty());
    }
}
