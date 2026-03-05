use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tirea_state::{GSet, State};

/// Tool permission behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionBehavior {
    Allow,
    #[default]
    Ask,
    Deny,
}

/// Public permission-domain action exposed to tools/plugins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionAction {
    SetDefault {
        behavior: ToolPermissionBehavior,
    },
    SetTool {
        tool_id: String,
        behavior: ToolPermissionBehavior,
    },
    RemoveTool {
        tool_id: String,
    },
    ClearTools,
}

/// Action type for the [`PermissionPolicy`] reducer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionPolicyAction {
    SetDefault { behavior: ToolPermissionBehavior },
    AllowTool { tool_id: String },
    DenyTool { tool_id: String },
}

/// Persisted sequential permission overrides (internal).
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[serde(default)]
#[tirea(path = "permissions", action = "PermissionAction", scope = "thread")]
pub(super) struct PermissionOverrides {
    pub default_behavior: ToolPermissionBehavior,
    pub tools: HashMap<String, ToolPermissionBehavior>,
}

impl PermissionOverrides {
    pub(super) fn reduce(&mut self, action: PermissionAction) {
        match action {
            PermissionAction::SetDefault { behavior } => self.default_behavior = behavior,
            PermissionAction::SetTool { tool_id, behavior } => {
                self.tools.insert(tool_id, behavior);
            }
            PermissionAction::RemoveTool { tool_id } => {
                self.tools.remove(&tool_id);
            }
            PermissionAction::ClearTools => self.tools.clear(),
        }
    }
}

/// Run-scoped CRDT permission policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[serde(default)]
#[tirea(
    path = "permission_policy",
    action = "PermissionPolicyAction",
    scope = "thread"
)]
pub struct PermissionPolicy {
    pub default_behavior: ToolPermissionBehavior,
    #[tirea(lattice)]
    pub allowed_tools: GSet<String>,
    #[tirea(lattice)]
    pub denied_tools: GSet<String>,
}

impl PermissionPolicy {
    pub(super) fn reduce(&mut self, action: PermissionPolicyAction) {
        match action {
            PermissionPolicyAction::SetDefault { behavior } => self.default_behavior = behavior,
            PermissionPolicyAction::AllowTool { tool_id } => {
                self.allowed_tools.insert(tool_id);
            }
            PermissionPolicyAction::DenyTool { tool_id } => {
                self.denied_tools.insert(tool_id);
            }
        }
    }
}

/// Route a [`PermissionAction`] to the correct state type.
///
/// `SetTool { Allow/Deny }` go to the CRDT [`PermissionPolicy`];
/// all other variants go through sequential [`PermissionOverrides`].
pub fn permission_state_action(
    action: PermissionAction,
) -> tirea_contract::runtime::state::AnyStateAction {
    use tirea_contract::runtime::state::AnyStateAction;
    match action {
        PermissionAction::SetTool {
            tool_id,
            behavior: ToolPermissionBehavior::Allow,
        } => AnyStateAction::new::<PermissionPolicy>(PermissionPolicyAction::AllowTool { tool_id }),
        PermissionAction::SetTool {
            tool_id,
            behavior: ToolPermissionBehavior::Deny,
        } => AnyStateAction::new::<PermissionPolicy>(PermissionPolicyAction::DenyTool { tool_id }),
        other => AnyStateAction::new::<PermissionOverrides>(other),
    }
}

/// Resolve effective permission behavior from a state snapshot.
///
/// Resolution order:
/// 1. CRDT policy (`permission_policy`): denied_tools → allowed_tools
/// 2. Legacy per-tool overrides (`permissions.tools`)
/// 3. `permission_policy.default_behavior` or `permissions.default_behavior`
/// 4. Default: `Ask`
#[must_use]
pub fn resolve_permission_behavior(
    snapshot: &serde_json::Value,
    tool_id: &str,
) -> ToolPermissionBehavior {
    if let Some(policy) = snapshot
        .get(PermissionPolicy::PATH)
        .and_then(|v| PermissionPolicy::from_value(v).ok())
    {
        let tool_id_owned = tool_id.to_string();
        if policy.denied_tools.contains(&tool_id_owned) {
            return ToolPermissionBehavior::Deny;
        }
        if policy.allowed_tools.contains(&tool_id_owned) {
            return ToolPermissionBehavior::Allow;
        }
    }

    let perms_value = snapshot.get(PermissionOverrides::PATH);
    if let Some(perms) = perms_value.and_then(|v| PermissionOverrides::from_value(v).ok()) {
        if let Some(&behavior) = perms.tools.get(tool_id) {
            return behavior;
        }
        return perms.default_behavior;
    }

    if let Some(policy) = snapshot
        .get(PermissionPolicy::PATH)
        .and_then(|v| PermissionPolicy::from_value(v).ok())
    {
        return policy.default_behavior;
    }

    perms_value
        .and_then(|v| v.get("default_behavior"))
        .and_then(|v| serde_json::from_value::<ToolPermissionBehavior>(v.clone()).ok())
        .unwrap_or_default()
}
