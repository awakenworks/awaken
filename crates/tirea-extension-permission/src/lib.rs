//! Permission policy extension.
//!
//! External callers use [`PermissionAction`], [`permission_state_action`],
//! [`PermissionPlugin`], and [`ToolPolicyPlugin`].

mod actions;
mod config;
mod form;
pub mod matcher;
mod mechanism;
mod model;
pub mod parser;
pub mod pattern;
mod plugin;
mod state;
mod strategy;

pub use actions::{
    apply_tool_policy, deny, deny_missing_call_id, deny_tool, reject_out_of_scope,
    request_permission,
};
pub use config::PermissionRulesConfig;
pub use form::{
    permission_confirmation_ticket, permission_confirmation_ticket_with_rule,
    PERMISSION_CONFIRM_TOOL_NAME,
};
pub use mechanism::{
    enforce_permission, remembered_permission_state_action, PermissionMechanismDecision,
    PermissionMechanismInput,
};
pub use model::{
    PermissionEvaluation, PermissionRule, PermissionRuleScope, PermissionRuleSource,
    PermissionRuleset, PermissionSubject, ToolPermissionBehavior,
};
pub use parser::parse_pattern;
pub use pattern::{ArgMatcher, FieldCondition, MatchOp, PathSegment, ToolCallPattern, ToolMatcher};
pub use plugin::{PermissionPlugin, ToolPolicyPlugin, PERMISSION_PLUGIN_ID};
pub use state::{
    permission_override_action, permission_rules_from_snapshot, permission_state_action,
    permission_update, PermissionAction, PermissionDestination, PermissionOverrideGranter,
    PermissionOverrides, PermissionPolicy,
};
pub use strategy::{evaluate_tool_permission, resolve_permission_behavior};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::io::ResumeDecisionAction;
    use tirea_contract::runtime::behavior::AgentBehavior;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::runtime::phase::{ActionSet, BeforeToolExecuteAction};
    use tirea_contract::runtime::tool_call::ToolCallResume;
    use tirea_contract::thread::ToolCall;
    use tirea_contract::RunPolicy;
    use tirea_contract::{Suspension, ToolCallDecision};
    use tirea_state::{DocCell, State};

    fn has_block(actions: &ActionSet<BeforeToolExecuteAction>) -> bool {
        actions
            .as_slice()
            .iter()
            .any(|a| matches!(a, BeforeToolExecuteAction::Block(_)))
    }

    fn suspend_action(
        actions: &ActionSet<BeforeToolExecuteAction>,
    ) -> Option<&tirea_contract::runtime::phase::SuspendTicket> {
        actions.as_slice().iter().find_map(|a| match a {
            BeforeToolExecuteAction::Suspend(ticket) => Some(ticket),
            _ => None,
        })
    }

    #[test]
    fn permission_policy_defaults_to_ask_with_no_rules() {
        let state = PermissionPolicy::default();
        assert_eq!(state.default_behavior, ToolPermissionBehavior::Ask);
        assert!(state.rules.is_empty());
    }

    #[test]
    fn permission_state_action_routes_to_policy_state() {
        let action = PermissionAction::SetTool {
            tool_id: "read".to_string(),
            behavior: ToolPermissionBehavior::Allow,
        };
        let serialized = permission_state_action(action).to_serialized_state_action();
        assert_eq!(serialized.base_path, PermissionPolicy::PATH);
        assert!(serialized.payload.is_object());
    }

    #[test]
    fn permission_rules_from_snapshot_reads_new_policy_rules() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {
                    "tool:read_file": {
                        "subject": { "kind": "tool", "tool_id": "read_file" },
                        "behavior": "allow",
                        "scope": "thread",
                        "source": "runtime"
                    }
                }
            }
        });

        let ruleset = permission_rules_from_snapshot(&snapshot);
        assert_eq!(ruleset.default_behavior, ToolPermissionBehavior::Deny);
        assert_eq!(
            ruleset.rule_for_tool("read_file").map(|rule| rule.behavior),
            Some(ToolPermissionBehavior::Allow)
        );
    }

    #[test]
    fn permission_rules_from_snapshot_reads_legacy_policy_shape() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "ask",
                "allowed_tools": ["read_file"],
                "denied_tools": ["write_file"]
            }
        });

        let ruleset = permission_rules_from_snapshot(&snapshot);
        assert_eq!(
            ruleset.rule_for_tool("read_file").map(|rule| rule.behavior),
            Some(ToolPermissionBehavior::Allow)
        );
        assert_eq!(
            ruleset
                .rule_for_tool("write_file")
                .map(|rule| rule.behavior),
            Some(ToolPermissionBehavior::Deny)
        );
    }

    #[test]
    fn permission_rules_from_snapshot_falls_back_to_legacy_permissions() {
        let snapshot = json!({
            "permissions": {
                "default_behavior": "deny",
                "tools": {
                    "recover_agent_run": "allow"
                }
            }
        });

        let ruleset = permission_rules_from_snapshot(&snapshot);
        assert_eq!(ruleset.default_behavior, ToolPermissionBehavior::Deny);
        assert_eq!(
            ruleset
                .rule_for_tool("recover_agent_run")
                .map(|rule| rule.behavior),
            Some(ToolPermissionBehavior::Allow)
        );
    }

    #[test]
    fn resolve_permission_prefers_new_policy_over_legacy_permissions() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "ask",
                "rules": {
                    "tool:write_file": {
                        "subject": { "kind": "tool", "tool_id": "write_file" },
                        "behavior": "deny",
                        "scope": "thread",
                        "source": "runtime"
                    }
                }
            },
            "permissions": {
                "default_behavior": "allow",
                "tools": {
                    "write_file": "allow"
                }
            }
        });

        assert_eq!(
            resolve_permission_behavior(&snapshot, "write_file", &json!({})),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn permission_confirmation_ticket_carries_message_and_schema() {
        let ticket =
            permission_confirmation_ticket("call_1", "write_file", json!({"path": "a.txt"}));
        assert_eq!(ticket.suspension.action, "tool:PermissionConfirm");
        assert_eq!(ticket.pending.name, "PermissionConfirm");
        assert_eq!(ticket.pending.arguments["tool_name"], "write_file");
        assert!(ticket.suspension.message.contains("write_file"));
        assert!(ticket.suspension.response_schema.is_some());
    }

    #[test]
    fn remembered_permission_state_action_persists_allow() {
        let tool_call = ToolCall::new("call_1", "write_file", json!({"path": "a.txt"}));
        let suspended_call = tirea_contract::runtime::SuspendedCall::new(
            &tool_call,
            permission_confirmation_ticket("call_1", "write_file", json!({"path": "a.txt"})),
        );
        let decision =
            ToolCallDecision::resume("fc_call_1", json!({"approved": true, "remember": true}), 1);

        let action = remembered_permission_state_action(&suspended_call, &decision)
            .expect("remembered approval should persist a rule");
        let serialized = action.to_serialized_state_action();
        assert_eq!(serialized.base_path, PermissionPolicy::PATH);
        assert_eq!(serialized.payload["behavior"], json!("allow"));
    }

    #[test]
    fn remembered_permission_state_action_persists_deny() {
        let tool_call = ToolCall::new("call_1", "write_file", json!({"path": "a.txt"}));
        let suspended_call = tirea_contract::runtime::SuspendedCall::new(
            &tool_call,
            permission_confirmation_ticket("call_1", "write_file", json!({"path": "a.txt"})),
        );
        let decision = ToolCallDecision::cancel(
            "fc_call_1",
            json!({"approved": false, "remember": true}),
            Some("denied".to_string()),
            1,
        );

        let action = remembered_permission_state_action(&suspended_call, &decision)
            .expect("remembered denial should persist a rule");
        let serialized = action.to_serialized_state_action();
        assert_eq!(serialized.base_path, PermissionPolicy::PATH);
        assert_eq!(serialized.payload["behavior"], json!("deny"));
    }

    #[test]
    fn remembered_permission_state_action_ignores_non_permission_suspensions() {
        let tool_call = ToolCall::new("call_1", "write_file", json!({"path": "a.txt"}));
        let suspended_call = tirea_contract::runtime::SuspendedCall::new(
            &tool_call,
            tirea_contract::runtime::SuspendTicket::new(
                Suspension::new("call_1", "tool:askUserQuestion"),
                tirea_contract::runtime::PendingToolCall::new(
                    "call_1",
                    "askUserQuestion",
                    json!({}),
                ),
                tirea_contract::runtime::ToolCallResumeMode::ReplayToolCall,
            ),
        );
        let decision =
            ToolCallDecision::resume("call_1", json!({"approved": true, "remember": true}), 1);

        assert!(remembered_permission_state_action(&suspended_call, &decision).is_none());
    }

    #[test]
    fn enforce_permission_ask_without_call_id_blocks() {
        let evaluation = PermissionEvaluation {
            subject: PermissionSubject::tool("write_file"),
            behavior: ToolPermissionBehavior::Ask,
            matched_rule: None,
        };
        let outcome = enforce_permission(
            PermissionMechanismInput {
                tool_id: "write_file",
                tool_args: json!({}),
                call_id: None,
                resume_action: None,
            },
            &evaluation,
        );
        assert!(matches!(
            outcome,
            PermissionMechanismDecision::Action(ref action) if matches!(**action, BeforeToolExecuteAction::Block(_))
        ));
    }

    fn read_only_ctx<'a>(
        config: &'a RunPolicy,
        doc: &'a DocCell,
        tool_name: &'a str,
        call_id: &'a str,
        args: &'a serde_json::Value,
    ) -> tirea_contract::runtime::behavior::ReadOnlyContext<'a> {
        tirea_contract::runtime::behavior::ReadOnlyContext::new(
            Phase::BeforeToolExecute,
            "t1",
            &[],
            config,
            doc,
        )
        .with_tool_info(tool_name, call_id, Some(args))
    }

    #[tokio::test]
    async fn permission_plugin_allow() {
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "permission_policy": {
                "default_behavior": "allow",
                "rules": {}
            }
        }));
        let args = json!({});
        let ctx = read_only_ctx(&config, &doc, "any_tool", "call_1", &args);
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&actions));
        assert!(suspend_action(&actions).is_none());
    }

    #[tokio::test]
    async fn permission_plugin_deny() {
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {}
            }
        }));
        let args = json!({});
        let ctx = read_only_ctx(&config, &doc, "any_tool", "call_1", &args);
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(has_block(&actions));
    }

    #[tokio::test]
    async fn permission_plugin_ask_suspends_with_tool_like_form() {
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "permission_policy": {
                "default_behavior": "ask",
                "rules": {}
            }
        }));
        let args = json!({"path": "a.txt"});
        let ctx = read_only_ctx(&config, &doc, "test_tool", "call_1", &args);
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        let ticket = suspend_action(&actions).expect("permission ask should suspend");
        assert_eq!(ticket.pending.name, "PermissionConfirm");
        assert_eq!(ticket.pending.arguments["tool_name"], "test_tool");
        assert!(ticket.suspension.message.contains("test_tool"));
        assert!(ticket.suspension.response_schema.is_some());
    }

    #[tokio::test]
    async fn permission_plugin_resume_bypasses_follow_up_prompt() {
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "permission_policy": {
                "default_behavior": "ask",
                "rules": {}
            }
        }));
        let args = json!({});
        let resume = ToolCallResume {
            decision_id: "decision_fc_call_1".to_string(),
            action: ResumeDecisionAction::Resume,
            result: serde_json::Value::Bool(true),
            reason: None,
            updated_at: 1,
        };
        let ctx =
            read_only_ctx(&config, &doc, "test_tool", "call_1", &args).with_resume_input(resume);
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        assert!(!has_block(&actions));
        assert!(suspend_action(&actions).is_none());
    }

    #[test]
    fn tool_policy_plugin_id() {
        assert_eq!(AgentBehavior::id(&ToolPolicyPlugin), "tool_policy");
    }

    #[tokio::test]
    async fn tool_policy_blocks_out_of_scope() {
        let mut config = RunPolicy::new();
        config.set_allowed_tools_if_absent(Some(&["other_tool".to_string()]));
        let doc = DocCell::new(json!({}));
        let args = json!({});
        let ctx = read_only_ctx(&config, &doc, "blocked_tool", "call_1", &args);
        let actions = AgentBehavior::before_tool_execute(&ToolPolicyPlugin, &ctx).await;
        assert!(has_block(&actions));
    }

    // --- Run-scoped permission overrides ---

    #[test]
    fn permission_override_action_routes_to_overrides_state() {
        let action = PermissionAction::SetTool {
            tool_id: "Bash".to_string(),
            behavior: ToolPermissionBehavior::Allow,
        };
        let serialized = permission_override_action(action).to_serialized_state_action();
        assert_eq!(serialized.base_path, PermissionOverrides::PATH);
        assert!(serialized.payload.is_object());
    }

    #[test]
    fn permission_override_set_default_falls_through_to_policy() {
        let action = PermissionAction::SetDefault {
            behavior: ToolPermissionBehavior::Allow,
        };
        let serialized = permission_override_action(action).to_serialized_state_action();
        // SetDefault has no run-scoped equivalent; must route to thread-level policy.
        assert_eq!(serialized.base_path, PermissionPolicy::PATH);
    }

    #[test]
    fn permission_overrides_take_precedence_over_policy() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {
                    "tool:Bash": {
                        "subject": { "kind": "tool", "tool_id": "Bash" },
                        "behavior": "deny",
                        "scope": "thread",
                        "source": "runtime"
                    }
                }
            },
            "permission_overrides": {
                "rules": {
                    "tool:Bash": {
                        "subject": { "kind": "tool", "tool_id": "Bash" },
                        "behavior": "allow",
                        "scope": "thread",
                        "source": "skill"
                    }
                }
            }
        });

        // Run-scoped override (allow) should win over thread-level policy (deny).
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({})),
            ToolPermissionBehavior::Allow
        );
    }

    #[test]
    fn permission_overrides_do_not_affect_unmatched_tools() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {}
            },
            "permission_overrides": {
                "rules": {
                    "tool:Bash": {
                        "subject": { "kind": "tool", "tool_id": "Bash" },
                        "behavior": "allow",
                        "scope": "thread",
                        "source": "skill"
                    }
                }
            }
        });

        // Bash is overridden to allow.
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({})),
            ToolPermissionBehavior::Allow
        );
        // Other tools still fall through to the policy default (deny).
        assert_eq!(
            resolve_permission_behavior(&snapshot, "write_file", &json!({})),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn permission_overrides_absent_leaves_policy_unchanged() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "ask",
                "rules": {
                    "tool:read_file": {
                        "subject": { "kind": "tool", "tool_id": "read_file" },
                        "behavior": "allow",
                        "scope": "thread",
                        "source": "runtime"
                    }
                }
            }
        });

        // No overrides present — behavior is unchanged.
        assert_eq!(
            resolve_permission_behavior(&snapshot, "read_file", &json!({})),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            resolve_permission_behavior(&snapshot, "unknown", &json!({})),
            ToolPermissionBehavior::Ask
        );
    }

    #[tokio::test]
    async fn permission_plugin_uses_run_scoped_overrides() {
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {}
            },
            "permission_overrides": {
                "rules": {
                    "tool:Bash": {
                        "subject": { "kind": "tool", "tool_id": "Bash" },
                        "behavior": "allow",
                        "scope": "thread",
                        "source": "skill"
                    }
                }
            }
        }));
        let args = json!({});
        let ctx = read_only_ctx(&config, &doc, "Bash", "call_1", &args);
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Override grants allow — no block, no suspend.
        assert!(!has_block(&actions));
        assert!(suspend_action(&actions).is_none());
    }

    #[tokio::test]
    async fn permission_plugin_denies_non_overridden_tool_with_deny_default() {
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {}
            },
            "permission_overrides": {
                "rules": {
                    "tool:Bash": {
                        "subject": { "kind": "tool", "tool_id": "Bash" },
                        "behavior": "allow",
                        "scope": "thread",
                        "source": "skill"
                    }
                }
            }
        }));
        let args = json!({});
        let ctx = read_only_ctx(&config, &doc, "write_file", "call_1", &args);
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // write_file has no override — falls through to policy default (deny).
        assert!(has_block(&actions));
    }

    #[test]
    fn permission_overrides_state_is_run_scoped() {
        use tirea_state::StateSpec;
        assert_eq!(PermissionOverrides::SCOPE, tirea_state::StateScope::Run,);
    }

    #[test]
    fn permission_overrides_default_is_empty() {
        let overrides = PermissionOverrides::default();
        assert!(overrides.rules.is_empty());
    }

    #[test]
    fn permission_override_reducer_marks_source_as_skill() {
        use tirea_contract::runtime::state::{reduce_state_actions, ScopeContext};
        use tirea_state::apply_patch;

        let base = json!({});
        let action = permission_override_action(PermissionAction::SetTool {
            tool_id: "Bash".to_string(),
            behavior: ToolPermissionBehavior::Allow,
        });
        let patches =
            reduce_state_actions(vec![action], &base, "test", &ScopeContext::run()).unwrap();
        let result = apply_patch(&base, patches[0].patch()).unwrap();
        let rule = &result["permission_overrides"]["rules"]["tool:Bash"];
        assert_eq!(rule["source"], "skill");
    }

    // --- Pattern-based permission rules ---

    #[test]
    fn pattern_rule_allows_matching_args() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {
                    "pattern:Bash(npm *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(npm *)" },
                        "behavior": "allow",
                        "scope": "thread",
                        "source": "runtime"
                    }
                }
            }
        });

        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({"command": "npm install"})),
            ToolPermissionBehavior::Allow
        );
        // Non-matching args fall through to default deny.
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({"command": "rm -rf /"})),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn pattern_rule_denies_dangerous_commands() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "allow",
                "rules": {
                    "pattern:Bash(rm *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(rm *)" },
                        "behavior": "deny"
                    }
                }
            }
        });

        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({"command": "rm -rf /tmp"})),
            ToolPermissionBehavior::Deny
        );
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({"command": "ls"})),
            ToolPermissionBehavior::Allow
        );
    }

    #[test]
    fn pattern_rule_glob_tool_name() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {
                    "pattern:mcp__github__*": {
                        "subject": { "kind": "pattern", "pattern": "mcp__github__*" },
                        "behavior": "allow"
                    }
                }
            }
        });

        assert_eq!(
            resolve_permission_behavior(&snapshot, "mcp__github__create_issue", &json!({})),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            resolve_permission_behavior(&snapshot, "mcp__slack__post", &json!({})),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn pattern_rule_named_field_glob() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {
                    "pattern:Edit(file_path ~ \"src/**\")": {
                        "subject": {
                            "kind": "pattern",
                            "pattern": "Edit(file_path ~ \"src/**\")"
                        },
                        "behavior": "allow"
                    }
                }
            }
        });

        assert_eq!(
            resolve_permission_behavior(&snapshot, "Edit", &json!({"file_path": "src/main.rs"})),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Edit", &json!({"file_path": "tests/test.rs"})),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn deny_pattern_beats_allow_pattern_same_specificity() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "ask",
                "rules": {
                    "pattern:Bash(npm *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(npm *)" },
                        "behavior": "allow"
                    },
                    "pattern:Bash(npm audit *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(npm audit *)" },
                        "behavior": "deny"
                    }
                }
            }
        });

        // "npm install" matches allow rule only
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({"command": "npm install"})),
            ToolPermissionBehavior::Allow
        );
        // "npm audit fix" matches both → deny wins
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({"command": "npm audit fix"})),
            ToolPermissionBehavior::Deny
        );
    }

    #[test]
    fn set_rule_action_persists_pattern() {
        let action = PermissionAction::SetRule {
            pattern: "Bash(npm *)".to_string(),
            behavior: ToolPermissionBehavior::Allow,
        };
        let serialized = permission_state_action(action).to_serialized_state_action();
        assert_eq!(serialized.base_path, PermissionPolicy::PATH);
        assert_eq!(serialized.payload["pattern"], "Bash(npm *)");
    }

    #[tokio::test]
    async fn permission_plugin_pattern_allows_matching_args() {
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {
                    "pattern:Bash(npm *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(npm *)" },
                        "behavior": "allow"
                    }
                }
            }
        }));
        let args = json!({"command": "npm install"});
        let ctx = read_only_ctx(&config, &doc, "Bash", "call_1", &args);
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Pattern matches → allow.
        assert!(!has_block(&actions));
        assert!(suspend_action(&actions).is_none());
    }

    #[tokio::test]
    async fn permission_plugin_pattern_denies_non_matching_args() {
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {
                    "pattern:Bash(npm *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(npm *)" },
                        "behavior": "allow"
                    }
                }
            }
        }));
        let args = json!({"command": "rm -rf /"});
        let ctx = read_only_ctx(&config, &doc, "Bash", "call_1", &args);
        let actions = AgentBehavior::before_tool_execute(&PermissionPlugin, &ctx).await;
        // Pattern doesn't match → falls to default deny.
        assert!(has_block(&actions));
    }

    #[test]
    fn mixed_old_and_new_rules_coexist() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "deny",
                "rules": {
                    "tool:Read": {
                        "subject": { "kind": "tool", "tool_id": "Read" },
                        "behavior": "allow"
                    },
                    "pattern:Bash(npm *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(npm *)" },
                        "behavior": "allow"
                    }
                }
            }
        });

        // Old tool rule works
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Read", &json!({})),
            ToolPermissionBehavior::Allow
        );
        // New pattern rule works
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({"command": "npm install"})),
            ToolPermissionBehavior::Allow
        );
        // Non-matching tool falls to default
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Write", &json!({})),
            ToolPermissionBehavior::Deny
        );
    }

    // --- Evaluation order: Deny > Allow > Ask ---

    #[test]
    fn allow_beats_ask_when_both_match() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "ask",
                "rules": {
                    "pattern:Bash(npm *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(npm *)" },
                        "behavior": "allow"
                    },
                    "pattern:Bash(npm install *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(npm install *)" },
                        "behavior": "ask"
                    }
                }
            }
        });

        // "npm install foo" matches both allow and ask → allow wins (Allow > Ask).
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({"command": "npm install foo"})),
            ToolPermissionBehavior::Allow
        );
    }

    #[test]
    fn deny_beats_allow_when_both_match() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "ask",
                "rules": {
                    "pattern:Bash(npm *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(npm *)" },
                        "behavior": "allow"
                    },
                    "pattern:Bash(npm audit *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(npm audit *)" },
                        "behavior": "deny"
                    }
                }
            }
        });

        // "npm audit fix" matches both allow and deny → deny wins (Deny > Allow).
        assert_eq!(
            resolve_permission_behavior(&snapshot, "Bash", &json!({"command": "npm audit fix"})),
            ToolPermissionBehavior::Deny
        );
    }

    // --- Unconditionally denied tools → visibility filtering ---

    #[test]
    fn unconditionally_denied_tools_from_legacy_rules() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "ask",
                "rules": {
                    "tool:Write": {
                        "subject": { "kind": "tool", "tool_id": "Write" },
                        "behavior": "deny"
                    },
                    "tool:Read": {
                        "subject": { "kind": "tool", "tool_id": "Read" },
                        "behavior": "allow"
                    }
                }
            }
        });

        let ruleset = permission_rules_from_snapshot(&snapshot);
        let denied = ruleset.unconditionally_denied_tools();
        assert!(denied.contains(&"Write"));
        assert!(!denied.contains(&"Read"));
    }

    #[test]
    fn unconditionally_denied_tools_from_pattern_rules() {
        let snapshot = json!({
            "permission_policy": {
                "default_behavior": "ask",
                "rules": {
                    "pattern:mcp__untrusted__*": {
                        "subject": { "kind": "pattern", "pattern": "mcp__untrusted__*" },
                        "behavior": "deny"
                    },
                    "pattern:Bash": {
                        "subject": { "kind": "pattern", "pattern": "Bash" },
                        "behavior": "deny"
                    },
                    "pattern:Bash(rm *)": {
                        "subject": { "kind": "pattern", "pattern": "Bash(rm *)" },
                        "behavior": "deny"
                    }
                }
            }
        });

        let ruleset = permission_rules_from_snapshot(&snapshot);
        let denied = ruleset.unconditionally_denied_tools();
        // "Bash" exact + no args → unconditionally denied → excluded from LLM.
        assert!(denied.contains(&"Bash"));
        // "Bash(rm *)" has arg condition → conditional deny → stays visible.
        // "mcp__untrusted__*" is glob → not exact → not in unconditional list.
        assert!(!denied.contains(&"mcp__untrusted__*"));
    }

    // --- Definition source + unified dispatch ---

    #[test]
    fn config_layer_rules_carry_definition_source_through_merge() {
        let rule = PermissionRule::new_tool("echo", ToolPermissionBehavior::Allow)
            .with_source(PermissionRuleSource::Definition);
        let config_rules = PermissionRulesConfig::new(vec![rule]);

        let mut ruleset = permission_rules_from_snapshot(&json!({}));
        // Simulate what PermissionPlugin.merged_ruleset does for config-layer.
        for rule in config_rules.rules() {
            ruleset
                .rules
                .entry(rule.subject.key())
                .or_insert_with(|| rule.clone());
        }

        let found = ruleset.rule_for_tool("echo").expect("rule must exist");
        assert_eq!(found.source, PermissionRuleSource::Definition);
    }

    #[test]
    fn permission_update_policy_routes_to_permission_policy() {
        let action = PermissionAction::SetTool {
            tool_id: "write".to_string(),
            behavior: ToolPermissionBehavior::Allow,
        };
        let serialized =
            permission_update(action, PermissionDestination::Policy).to_serialized_state_action();
        assert_eq!(serialized.base_path, PermissionPolicy::PATH);
    }

    #[test]
    fn permission_update_override_routes_to_permission_overrides() {
        let action = PermissionAction::SetTool {
            tool_id: "Bash".to_string(),
            behavior: ToolPermissionBehavior::Allow,
        };
        let serialized =
            permission_update(action, PermissionDestination::Override).to_serialized_state_action();
        assert_eq!(serialized.base_path, PermissionOverrides::PATH);
    }

    #[test]
    fn permission_update_matches_direct_state_action() {
        let action_a = PermissionAction::SetTool {
            tool_id: "read".to_string(),
            behavior: ToolPermissionBehavior::Allow,
        };
        let action_b = action_a.clone();
        let via_update =
            permission_update(action_a, PermissionDestination::Policy).to_serialized_state_action();
        let via_direct = permission_state_action(action_b).to_serialized_state_action();
        assert_eq!(via_update.base_path, via_direct.base_path);
        assert_eq!(via_update.payload, via_direct.payload);
    }

    #[test]
    fn permission_update_override_matches_direct_override_action() {
        let action_a = PermissionAction::SetTool {
            tool_id: "Bash".to_string(),
            behavior: ToolPermissionBehavior::Allow,
        };
        let action_b = action_a.clone();
        let via_update = permission_update(action_a, PermissionDestination::Override)
            .to_serialized_state_action();
        let via_direct = permission_override_action(action_b).to_serialized_state_action();
        assert_eq!(via_update.base_path, via_direct.base_path);
        assert_eq!(via_update.payload, via_direct.payload);
    }
}
