use async_trait::async_trait;

use awaken_runtime::agent::state::ExcludeTool;
use awaken_runtime::state::StateCommand;
use awaken_runtime::{PhaseContext, PhaseHook};
use awaken_runtime_contract::StateError;

use crate::state::{PermissionOverridesKey, PermissionPolicyKey, permission_rules_from_state};

/// BeforeInference hook that removes unconditionally-denied tools from the
/// tool list before the LLM sees them.
///
/// Two flavors of "unconditional Deny" are stripped here:
///
///  - **Exact-tool Deny** (`{ tool: "rm", deny }` or
///    `{ tool: "rm()", deny }`) — handled by
///    `PermissionRuleset::unconditionally_denied_tools` regardless of the
///    registry snapshot.
///  - **Glob/Regex Deny + any-args** (`{ tool: "mcp__db__*", deny }`) —
///    expanded against the live tool registry snapshot from
///    `PhaseContext::registry_snapshot`. Each registered tool id matched by
///    the pattern is scheduled as an `ExcludeTool` so the model never sees
///    those tools. The admin-console permission preview performs the same
///    expansion via the shared
///    `PermissionRuleset::unconditionally_denied_against` helper — the
///    preview claim "X tools stripped before the model sees the list" must
///    match the runtime, or the UI lies about what BeforeInference does.
///
/// Per-args patterns (`Bash(npm *)` etc.) remain conditional and are
/// handled at `ToolGate` by [`super::checker::PermissionToolGateHook`].
///
/// When the context has no registry snapshot (minimal test contexts), the
/// glob/regex expansion is skipped and only exact-tool Deny applies. The
/// production runtime runner always attaches a snapshot.
pub(super) struct PermissionToolFilterHook;

#[async_trait]
impl PhaseHook for PermissionToolFilterHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let policy = ctx.state::<PermissionPolicyKey>();
        let overrides = ctx.state::<PermissionOverridesKey>();
        let ruleset = permission_rules_from_state(policy, overrides);

        let tool_ids: Vec<String> = ctx
            .registry_snapshot
            .as_ref()
            .map(|snapshot| snapshot.registries().tools.tool_ids())
            .unwrap_or_default();

        let denied = ruleset.unconditionally_denied_against(&tool_ids);
        if denied.is_empty() {
            return Ok(StateCommand::new());
        }

        let mut cmd = StateCommand::new();
        // Sort for deterministic ordering — `HashSet` iteration is otherwise
        // unstable, which would surface as flaky audit-log / test output.
        let mut denied: Vec<String> = denied.into_iter().collect();
        denied.sort();
        for tool_id in denied {
            cmd.schedule_action::<ExcludeTool>(tool_id)?;
        }
        Ok(cmd)
    }
}
