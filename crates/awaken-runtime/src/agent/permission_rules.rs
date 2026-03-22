//! Rules-based tool permission checker.
//!
//! Evaluates tool names against an ordered list of glob-pattern rules.
//! First matching rule wins; unmatched tools return `Abstain`.

use async_trait::async_trait;

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::runtime::{PhaseContext, ToolPermission, ToolPermissionChecker};
use awaken_contract::StateError;

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
                        message: None,
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
    use crate::state::{Snapshot, StateMap};
    use awaken_contract::model::Phase;
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
                reason: "destructive".into(),
                message: None,
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

    // -----------------------------------------------------------------------
    // Migrated from uncarve: additional permission rule tests
    // -----------------------------------------------------------------------

    #[test]
    fn glob_empty_pattern_matches_empty_name() {
        assert!(glob_matches("", ""));
    }

    #[test]
    fn glob_empty_pattern_does_not_match_nonempty() {
        assert!(!glob_matches("", "foo"));
    }

    #[test]
    fn glob_star_matches_empty_string() {
        assert!(glob_matches("*", ""));
    }

    #[test]
    fn glob_prefix_star_empty_suffix() {
        assert!(glob_matches("read_*", "read_"));
    }

    #[test]
    fn glob_star_suffix_empty_prefix() {
        assert!(glob_matches("*_tool", "_tool"));
    }

    #[test]
    fn glob_no_partial_match() {
        assert!(!glob_matches("foo", "foobar"));
        assert!(!glob_matches("foo", "afoo"));
    }

    #[tokio::test]
    async fn no_tool_name_in_context_abstains() {
        let checker =
            RulesPermissionChecker::new(vec![PermissionRule::new("*", PermissionAction::Allow)]);
        let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot());
        // No with_tool_info call
        let result = checker.check(&ctx).await.unwrap();
        assert_eq!(result, ToolPermission::Abstain);
    }

    #[tokio::test]
    async fn multiple_rules_first_match_only() {
        let checker = RulesPermissionChecker::new(vec![
            PermissionRule::new("read_*", PermissionAction::Allow),
            PermissionRule::new("read_secret", PermissionAction::Deny),
            PermissionRule::new("*", PermissionAction::Ask),
        ]);
        // read_secret matches "read_*" first (Allow), not "read_secret" (Deny)
        let result = checker.check(&ctx_with_tool("read_secret")).await.unwrap();
        assert_eq!(result, ToolPermission::Allow);
    }

    #[tokio::test]
    async fn deny_with_default_reason() {
        let checker = RulesPermissionChecker::new(vec![PermissionRule::new(
            "rm_file",
            PermissionAction::Deny,
        )]);
        let result = checker.check(&ctx_with_tool("rm_file")).await.unwrap();
        match result {
            ToolPermission::Deny { reason, .. } => {
                assert!(reason.contains("rm_file"));
            }
            _ => panic!("expected Deny"),
        }
    }

    #[tokio::test]
    async fn allow_all_with_wildcard() {
        let checker =
            RulesPermissionChecker::new(vec![PermissionRule::new("*", PermissionAction::Allow)]);

        for tool_name in ["search", "read_file", "execute_code", "rm_everything"] {
            let result = checker.check(&ctx_with_tool(tool_name)).await.unwrap();
            assert_eq!(
                result,
                ToolPermission::Allow,
                "wildcard should allow {}",
                tool_name
            );
        }
    }

    #[tokio::test]
    async fn deny_all_with_wildcard() {
        let checker = RulesPermissionChecker::new(vec![
            PermissionRule::new("*", PermissionAction::Deny).with_reason("all denied"),
        ]);

        let result = checker.check(&ctx_with_tool("anything")).await.unwrap();
        match result {
            ToolPermission::Deny { reason, .. } => {
                assert_eq!(reason, "all denied");
            }
            _ => panic!("expected Deny"),
        }
    }

    #[tokio::test]
    async fn complex_rule_chain() {
        let checker = RulesPermissionChecker::new(vec![
            PermissionRule::new("rm_*", PermissionAction::Deny).with_reason("destructive"),
            PermissionRule::new("write_*", PermissionAction::Ask),
            PermissionRule::new("read_*", PermissionAction::Allow),
            PermissionRule::new("*", PermissionAction::Ask),
        ]);

        let rm = checker.check(&ctx_with_tool("rm_file")).await.unwrap();
        assert!(rm.is_deny());

        let write = checker.check(&ctx_with_tool("write_file")).await.unwrap();
        assert_eq!(write, ToolPermission::Abstain);

        let read = checker.check(&ctx_with_tool("read_file")).await.unwrap();
        assert_eq!(read, ToolPermission::Allow);

        let other = checker.check(&ctx_with_tool("execute")).await.unwrap();
        assert_eq!(other, ToolPermission::Abstain); // wildcard Ask
    }

    #[test]
    fn permission_rule_builder() {
        let rule =
            PermissionRule::new("test_*", PermissionAction::Deny).with_reason("testing only");
        assert_eq!(rule.tool_pattern, "test_*");
        assert_eq!(rule.action, PermissionAction::Deny);
        assert_eq!(rule.reason.as_deref(), Some("testing only"));
    }

    #[test]
    fn rules_permission_plugin_descriptor() {
        let plugin = RulesPermissionPlugin::new(vec![]);
        assert_eq!(plugin.descriptor().name, "tool-permission:rules");
    }
}
