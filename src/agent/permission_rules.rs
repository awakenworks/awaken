//! Rules-based tool permission checker.
//!
//! Evaluates tool names against an ordered list of glob-pattern rules.
//! First matching rule wins; unmatched tools return `Abstain`.

use async_trait::async_trait;

use crate::error::StateError;
use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::runtime::{PhaseContext, ToolPermission, ToolPermissionChecker};

/// Decision a rule can produce.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionAction {
    Allow,
    Deny,
    /// Maps to `ToolPermission::Abstain`, triggering suspension via aggregation.
    Ask,
}

/// A single permission rule matching tool names by glob pattern.
#[derive(Debug, Clone)]
pub struct PermissionRule {
    /// Glob pattern: exact match, or `*` for single-segment wildcard.
    /// Examples: `"rm_file"`, `"read_*"`, `"*"`.
    pub tool_pattern: String,
    /// Action when the pattern matches.
    pub action: PermissionAction,
    /// Optional human-readable reason (used in Deny messages).
    pub reason: Option<String>,
}

impl PermissionRule {
    pub fn new(tool_pattern: impl Into<String>, action: PermissionAction) -> Self {
        Self {
            tool_pattern: tool_pattern.into(),
            action,
            reason: None,
        }
    }

    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// Check whether `name` matches a simple glob `pattern`.
///
/// Supported syntax:
/// - `*` alone matches everything
/// - `prefix*` matches names starting with `prefix`
/// - `*suffix` matches names ending with `suffix`
/// - `prefix*suffix` matches names starting with `prefix` and ending with `suffix`
/// - No `*` means exact match
fn glob_matches(pattern: &str, name: &str) -> bool {
    match pattern.find('*') {
        None => pattern == name,
        Some(star) => {
            let prefix = &pattern[..star];
            let suffix = &pattern[star + 1..];
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
    }
}

/// Ordered rule-set checker. Evaluates rules top-to-bottom; first match wins.
pub struct RulesPermissionChecker {
    rules: Vec<PermissionRule>,
}

impl RulesPermissionChecker {
    pub fn new(rules: Vec<PermissionRule>) -> Self {
        Self { rules }
    }
}

#[async_trait]
impl ToolPermissionChecker for RulesPermissionChecker {
    async fn check(&self, ctx: &PhaseContext) -> Result<ToolPermission, StateError> {
        let tool_name = match &ctx.tool_name {
            Some(name) => name.as_str(),
            None => return Ok(ToolPermission::Abstain),
        };

        for rule in &self.rules {
            if glob_matches(&rule.tool_pattern, tool_name) {
                return Ok(match &rule.action {
                    PermissionAction::Allow => ToolPermission::Allow,
                    PermissionAction::Deny => ToolPermission::Deny {
                        reason: rule
                            .reason
                            .clone()
                            .unwrap_or_else(|| format!("denied by rule: {}", rule.tool_pattern)),
                    },
                    PermissionAction::Ask => ToolPermission::Abstain,
                });
            }
        }

        // No rule matched — abstain (aggregation will suspend).
        Ok(ToolPermission::Abstain)
    }
}

/// Plugin that installs a [`RulesPermissionChecker`].
pub struct RulesPermissionPlugin {
    rules: Vec<PermissionRule>,
}

impl RulesPermissionPlugin {
    pub fn new(rules: Vec<PermissionRule>) -> Self {
        Self { rules }
    }
}

impl Plugin for RulesPermissionPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "tool-permission:rules",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_tool_permission(
            "tool-permission:rules",
            RulesPermissionChecker::new(self.rules.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Phase;
    use crate::state::{Snapshot, StateMap};
    use std::sync::Arc;

    fn empty_snapshot() -> Snapshot {
        Snapshot::new(0, Arc::new(StateMap::default()))
    }

    fn ctx_with_tool(name: &str) -> PhaseContext {
        PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
            .with_tool_info(name, "call-1", None)
    }

    #[tokio::test]
    async fn rule_exact_match_deny() {
        let checker = RulesPermissionChecker::new(vec![
            PermissionRule::new("rm_file", PermissionAction::Deny).with_reason("destructive"),
        ]);
        let result = checker.check(&ctx_with_tool("rm_file")).await.unwrap();
        assert_eq!(
            result,
            ToolPermission::Deny {
                reason: "destructive".into()
            }
        );
    }

    #[tokio::test]
    async fn rule_wildcard_allow() {
        let checker = RulesPermissionChecker::new(vec![PermissionRule::new(
            "read_*",
            PermissionAction::Allow,
        )]);
        let result = checker.check(&ctx_with_tool("read_file")).await.unwrap();
        assert_eq!(result, ToolPermission::Allow);

        let result = checker
            .check(&ctx_with_tool("read_database"))
            .await
            .unwrap();
        assert_eq!(result, ToolPermission::Allow);
    }

    #[tokio::test]
    async fn rule_first_match_wins() {
        let checker = RulesPermissionChecker::new(vec![
            PermissionRule::new("write_file", PermissionAction::Deny),
            PermissionRule::new("*", PermissionAction::Allow),
        ]);

        // write_file hits the deny rule first
        let result = checker.check(&ctx_with_tool("write_file")).await.unwrap();
        assert!(result.is_deny());

        // anything else hits the wildcard allow
        let result = checker.check(&ctx_with_tool("read_file")).await.unwrap();
        assert_eq!(result, ToolPermission::Allow);
    }

    #[tokio::test]
    async fn rule_ask_triggers_suspend() {
        let checker = RulesPermissionChecker::new(vec![PermissionRule::new(
            "execute_*",
            PermissionAction::Ask,
        )]);
        let result = checker.check(&ctx_with_tool("execute_code")).await.unwrap();
        assert_eq!(result, ToolPermission::Abstain);
    }

    #[tokio::test]
    async fn no_matching_rule_abstains() {
        let checker = RulesPermissionChecker::new(vec![PermissionRule::new(
            "rm_file",
            PermissionAction::Deny,
        )]);
        let result = checker.check(&ctx_with_tool("read_file")).await.unwrap();
        assert_eq!(result, ToolPermission::Abstain);
    }

    #[test]
    fn glob_exact_match() {
        assert!(glob_matches("foo", "foo"));
        assert!(!glob_matches("foo", "foobar"));
    }

    #[test]
    fn glob_star_alone() {
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("*", ""));
    }

    #[test]
    fn glob_prefix_star() {
        assert!(glob_matches("read_*", "read_file"));
        assert!(!glob_matches("read_*", "write_file"));
    }

    #[test]
    fn glob_star_suffix() {
        assert!(glob_matches("*_file", "read_file"));
        assert!(!glob_matches("*_file", "read_db"));
    }

    #[test]
    fn glob_prefix_star_suffix() {
        assert!(glob_matches("a*z", "abcz"));
        assert!(glob_matches("a*z", "az"));
        assert!(!glob_matches("a*z", "abcd"));
    }
}
