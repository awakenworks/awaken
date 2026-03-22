//! Convenience action constructors for permission state mutations.

use awaken_runtime::state::MutationBatch;

use crate::rules::ToolPermissionBehavior;
use crate::state::{PermissionAction, PermissionOverridesKey, PermissionPolicyKey};

/// Apply a permission action to the thread-scoped policy state.
pub fn apply_policy_action(batch: &mut MutationBatch, action: PermissionAction) {
    batch.update::<PermissionPolicyKey>(action);
}

/// Apply a permission action to the run-scoped overrides state.
pub fn apply_override_action(batch: &mut MutationBatch, action: PermissionAction) {
    batch.update::<PermissionOverridesKey>(action);
}

/// Set the default permission behavior for all tools.
pub fn set_default_behavior(batch: &mut MutationBatch, behavior: ToolPermissionBehavior) {
    apply_policy_action(batch, PermissionAction::SetDefault { behavior });
}

/// Allow a specific tool.
pub fn allow_tool(batch: &mut MutationBatch, tool_id: impl Into<String>) {
    apply_policy_action(
        batch,
        PermissionAction::AllowTool {
            tool_id: tool_id.into(),
        },
    );
}

/// Deny a specific tool.
pub fn deny_tool(batch: &mut MutationBatch, tool_id: impl Into<String>) {
    apply_policy_action(
        batch,
        PermissionAction::DenyTool {
            tool_id: tool_id.into(),
        },
    );
}

/// Set a pattern-based permission rule on the thread-scoped policy.
pub fn set_rule(
    batch: &mut MutationBatch,
    pattern: impl Into<String>,
    behavior: ToolPermissionBehavior,
) {
    apply_policy_action(
        batch,
        PermissionAction::SetRule {
            pattern: pattern.into(),
            behavior,
        },
    );
}

/// Remove a tool rule from the thread-scoped policy.
pub fn remove_tool(batch: &mut MutationBatch, tool_id: impl Into<String>) {
    apply_policy_action(
        batch,
        PermissionAction::RemoveTool {
            tool_id: tool_id.into(),
        },
    );
}

/// Remove a pattern rule from the thread-scoped policy.
pub fn remove_rule(batch: &mut MutationBatch, pattern: impl Into<String>) {
    apply_policy_action(
        batch,
        PermissionAction::RemoveRule {
            pattern: pattern.into(),
        },
    );
}

/// Clear all tool rules from the thread-scoped policy.
pub fn clear_tools(batch: &mut MutationBatch) {
    apply_policy_action(batch, PermissionAction::ClearTools);
}

/// Grant temporary tool access via run-scoped overrides.
pub fn grant_tool_override(batch: &mut MutationBatch, tool_id: impl Into<String>) {
    apply_override_action(
        batch,
        PermissionAction::AllowTool {
            tool_id: tool_id.into(),
        },
    );
}

/// Grant temporary pattern-based access via run-scoped overrides.
pub fn grant_rule_override(
    batch: &mut MutationBatch,
    pattern: impl Into<String>,
    behavior: ToolPermissionBehavior,
) {
    apply_override_action(
        batch,
        PermissionAction::SetRule {
            pattern: pattern.into(),
            behavior,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_tool_creates_mutation() {
        let mut batch = MutationBatch::new();
        allow_tool(&mut batch, "Bash");
        assert!(!batch.is_empty());
    }

    #[test]
    fn deny_tool_creates_mutation() {
        let mut batch = MutationBatch::new();
        deny_tool(&mut batch, "rm");
        assert!(!batch.is_empty());
    }

    #[test]
    fn set_rule_creates_mutation() {
        let mut batch = MutationBatch::new();
        set_rule(&mut batch, "Bash(npm *)", ToolPermissionBehavior::Allow);
        assert!(!batch.is_empty());
    }

    #[test]
    fn grant_tool_override_creates_mutation() {
        let mut batch = MutationBatch::new();
        grant_tool_override(&mut batch, "Bash");
        assert!(!batch.is_empty());
    }

    #[test]
    fn clear_tools_creates_mutation() {
        let mut batch = MutationBatch::new();
        clear_tools(&mut batch);
        assert!(!batch.is_empty());
    }
}
