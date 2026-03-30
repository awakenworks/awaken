use std::sync::Arc;

use serde_json::json;

use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::contract::tool::ToolResult;
use awaken_contract::model::Phase;
use awaken_contract::{Snapshot, StateMap};
use awaken_runtime::phase::{PhaseContext, PhaseHook};
use awaken_runtime::plugins::Plugin;
use awaken_tool_pattern::ToolCallPattern;

use super::hook::ReminderHook;
use super::plugin::{REMINDER_PLUGIN_NAME, ReminderPlugin};
use crate::output_matcher::OutputMatcher;
use crate::rule::ReminderRule;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn empty_snapshot() -> Snapshot {
    Snapshot::new(0, Arc::new(StateMap::default()))
}

fn make_rule(
    name: &str,
    pattern: ToolCallPattern,
    output: OutputMatcher,
    msg: &str,
) -> ReminderRule {
    ReminderRule {
        name: name.into(),
        pattern,
        output,
        message: ContextMessage::system(format!("reminder.{name}"), msg),
    }
}

fn after_tool_ctx(
    tool_name: &str,
    tool_args: serde_json::Value,
    result: ToolResult,
) -> PhaseContext {
    PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
        .with_tool_info(tool_name, "call-1", Some(tool_args))
        .with_tool_result(result)
}

// ---------------------------------------------------------------------------
// ReminderHook tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hook_produces_add_context_message_when_tool_matches() {
    let rules = vec![make_rule(
        "bash-warn",
        ToolCallPattern::tool("Bash"),
        OutputMatcher::Any,
        "Be careful with Bash",
    )];

    let hook = ReminderHook {
        rules: Arc::from(rules),
    };

    let ctx = after_tool_ctx("Bash", json!({}), ToolResult::success("Bash", json!("ok")));
    let cmd = hook.run(&ctx).await.unwrap();

    assert!(!cmd.is_empty(), "command should contain a scheduled action");
    assert_eq!(cmd.scheduled_actions().len(), 1);
}

#[tokio::test]
async fn hook_returns_empty_command_when_no_rules_match() {
    let rules = vec![make_rule(
        "edit-only",
        ToolCallPattern::tool("Edit"),
        OutputMatcher::Any,
        "Edit reminder",
    )];

    let hook = ReminderHook {
        rules: Arc::from(rules),
    };

    // Tool is Bash, but rule only matches Edit
    let ctx = after_tool_ctx("Bash", json!({}), ToolResult::success("Bash", json!("ok")));
    let cmd = hook.run(&ctx).await.unwrap();

    assert!(
        cmd.is_empty(),
        "command should be empty when no rules match"
    );
}

#[tokio::test]
async fn hook_handles_multiple_matching_rules() {
    let rules = vec![
        make_rule(
            "wildcard",
            ToolCallPattern::tool_glob("*"),
            OutputMatcher::Any,
            "Global reminder",
        ),
        make_rule(
            "bash-specific",
            ToolCallPattern::tool("Bash"),
            OutputMatcher::Any,
            "Bash reminder",
        ),
        make_rule(
            "edit-only",
            ToolCallPattern::tool("Edit"),
            OutputMatcher::Any,
            "Edit reminder",
        ),
    ];

    let hook = ReminderHook {
        rules: Arc::from(rules),
    };

    let ctx = after_tool_ctx("Bash", json!({}), ToolResult::success("Bash", json!("ok")));
    let cmd = hook.run(&ctx).await.unwrap();

    // Wildcard and Bash-specific should match; Edit-only should not
    assert_eq!(
        cmd.scheduled_actions().len(),
        2,
        "wildcard + bash-specific rules should both match"
    );
}

#[tokio::test]
async fn hook_returns_empty_when_tool_name_is_none() {
    let rules = vec![make_rule(
        "any-tool",
        ToolCallPattern::tool_glob("*"),
        OutputMatcher::Any,
        "reminder",
    )];

    let hook = ReminderHook {
        rules: Arc::from(rules),
    };

    // Context without tool_name set
    let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot());
    let cmd = hook.run(&ctx).await.unwrap();

    assert!(cmd.is_empty(), "should return empty when tool_name is None");
}

#[tokio::test]
async fn hook_returns_empty_when_tool_result_is_none() {
    let rules = vec![make_rule(
        "any-tool",
        ToolCallPattern::tool_glob("*"),
        OutputMatcher::Any,
        "reminder",
    )];

    let hook = ReminderHook {
        rules: Arc::from(rules),
    };

    // Context with tool_name but no tool_result
    let mut ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot());
    ctx.tool_name = Some("Bash".into());
    let cmd = hook.run(&ctx).await.unwrap();

    assert!(
        cmd.is_empty(),
        "should return empty when tool_result is None"
    );
}

#[tokio::test]
async fn hook_respects_output_matcher() {
    use crate::output_matcher::ToolStatusMatcher;

    let rules = vec![make_rule(
        "error-only",
        ToolCallPattern::tool_glob("*"),
        OutputMatcher::Status(ToolStatusMatcher::Error),
        "Tool failed",
    )];

    let hook = ReminderHook {
        rules: Arc::from(rules),
    };

    // Success result should not match an error-only rule
    let ctx_success = after_tool_ctx("Bash", json!({}), ToolResult::success("Bash", json!("ok")));
    let cmd = hook.run(&ctx_success).await.unwrap();
    assert!(
        cmd.is_empty(),
        "success result should not match error-only rule"
    );

    // Error result should match
    let ctx_error = after_tool_ctx("Bash", json!({}), ToolResult::error("Bash", "failed"));
    let cmd = hook.run(&ctx_error).await.unwrap();
    assert_eq!(
        cmd.scheduled_actions().len(),
        1,
        "error result should match"
    );
}

#[tokio::test]
async fn hook_with_empty_rules_returns_empty() {
    let hook = ReminderHook {
        rules: Arc::from(Vec::<ReminderRule>::new()),
    };

    let ctx = after_tool_ctx("Bash", json!({}), ToolResult::success("Bash", json!("ok")));
    let cmd = hook.run(&ctx).await.unwrap();

    assert!(cmd.is_empty());
}

// ---------------------------------------------------------------------------
// ReminderPlugin tests
// ---------------------------------------------------------------------------

#[test]
fn plugin_descriptor_has_correct_name() {
    let plugin = ReminderPlugin::new(vec![]);
    assert_eq!(plugin.descriptor().name, REMINDER_PLUGIN_NAME);
    assert_eq!(plugin.descriptor().name, "reminder");
}

#[test]
fn plugin_with_empty_rules() {
    let plugin = ReminderPlugin::new(vec![]);
    assert_eq!(plugin.rules.len(), 0);
}

#[test]
fn plugin_stores_provided_rules() {
    let rules = vec![
        make_rule(
            "r1",
            ToolCallPattern::tool("Bash"),
            OutputMatcher::Any,
            "msg1",
        ),
        make_rule(
            "r2",
            ToolCallPattern::tool("Edit"),
            OutputMatcher::Any,
            "msg2",
        ),
    ];

    let plugin = ReminderPlugin::new(rules);
    assert_eq!(plugin.rules.len(), 2);
}

#[test]
fn plugin_registers_hook_at_after_tool_execute_phase() {
    use awaken_runtime::plugins::PluginRegistrar;

    let plugin = ReminderPlugin::new(vec![make_rule(
        "test",
        ToolCallPattern::tool("Bash"),
        OutputMatcher::Any,
        "test msg",
    )]);

    let mut registrar = PluginRegistrar::new_for_test();
    let result = plugin.register(&mut registrar);
    assert!(result.is_ok(), "register should succeed");
}
