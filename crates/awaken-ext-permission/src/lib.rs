//! Permission extension for the awaken agent framework.
//!
//! Provides declarative permission rules with glob/regex pattern matching
//! on tool names and arguments. Rules are evaluated with firewall-like
//! priority: Deny > Allow > Ask.

pub mod actions;
pub mod config;
pub mod matcher;
pub mod plugin;
pub mod rules;
pub mod state;

pub use config::{
    PermissionConfigError, PermissionConfigKey, PermissionRuleEntry, PermissionRulesConfig,
};
pub use plugin::PermissionPlugin;
pub use rules::{
    ArgMatcher, FieldCondition, MatchOp, PathSegment, PatternParseError, PermissionEvaluation,
    PermissionRule, PermissionRuleScope, PermissionRuleSource, PermissionRuleset,
    PermissionSubject, ToolCallPattern, ToolMatcher, ToolPermissionBehavior,
    evaluate_tool_permission, parse_pattern,
};
pub use state::{
    PermissionAction, PermissionOverrides, PermissionOverridesKey, PermissionPolicy,
    PermissionPolicyKey, permission_rules_from_state,
};
