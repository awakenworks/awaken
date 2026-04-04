//! Integration tests for the reminder extension.

use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::contract::tool::ToolResult;
use awaken_ext_reminder::ReminderRulesConfig;
use awaken_ext_reminder::output_matcher::{
    ContentMatcher, OutputMatcher, ToolStatusMatcher, output_matches,
};
use awaken_ext_reminder::plugin::ReminderPlugin;
use awaken_ext_reminder::rule::ReminderRule;
use awaken_tool_pattern::{
    ArgMatcher, FieldCondition, MatchOp, PathSegment, ToolCallPattern, ToolMatcher, pattern_matches,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// Pattern matching (reuse tool_pattern from contract)
// ---------------------------------------------------------------------------

#[test]
fn pattern_exact_tool_matches() {
    let p = ToolCallPattern::tool("Bash");
    assert!(pattern_matches(&p, "Bash", &json!({})).is_match());
    assert!(!pattern_matches(&p, "Edit", &json!({})).is_match());
}

#[test]
fn pattern_glob_tool_matches() {
    let p = ToolCallPattern::tool_glob("mcp__*");
    assert!(pattern_matches(&p, "mcp__github__create", &json!({})).is_match());
    assert!(!pattern_matches(&p, "Bash", &json!({})).is_match());
}

#[test]
fn pattern_with_field_conditions() {
    let p = ToolCallPattern {
        tool: ToolMatcher::Exact("Bash".into()),
        args: ArgMatcher::Fields(vec![FieldCondition {
            path: vec![PathSegment::Field("command".into())],
            op: MatchOp::Glob,
            value: "rm *".into(),
        }]),
    };
    assert!(pattern_matches(&p, "Bash", &json!({"command": "rm -rf /"})).is_match());
    assert!(!pattern_matches(&p, "Bash", &json!({"command": "ls"})).is_match());
}

// ---------------------------------------------------------------------------
// Output matching — status
// ---------------------------------------------------------------------------

#[test]
fn output_status_success() {
    let m = OutputMatcher::Status(ToolStatusMatcher::Success);
    assert!(output_matches(&m, &ToolResult::success("t", json!("ok"))));
    assert!(!output_matches(&m, &ToolResult::error("t", "err")));
}

#[test]
fn output_status_error() {
    let m = OutputMatcher::Status(ToolStatusMatcher::Error);
    assert!(output_matches(&m, &ToolResult::error("t", "err")));
    assert!(!output_matches(&m, &ToolResult::success("t", json!("ok"))));
}

#[test]
fn output_status_pending() {
    let m = OutputMatcher::Status(ToolStatusMatcher::Pending);
    assert!(output_matches(&m, &ToolResult::suspended("t", "wait")));
    assert!(!output_matches(&m, &ToolResult::success("t", json!("ok"))));
}

// ---------------------------------------------------------------------------
// Output matching — text content
// ---------------------------------------------------------------------------

#[test]
fn output_text_glob() {
    let m = OutputMatcher::Content(ContentMatcher::Text {
        op: MatchOp::Glob,
        value: "*error*".into(),
    });
    assert!(output_matches(
        &m,
        &ToolResult::success("t", json!("some error occurred"))
    ));
    assert!(!output_matches(
        &m,
        &ToolResult::success("t", json!("all good"))
    ));
}

#[test]
fn output_text_regex() {
    let m = OutputMatcher::Content(ContentMatcher::Text {
        op: MatchOp::Regex,
        value: r"(?i)warning|alert".into(),
    });
    assert!(output_matches(
        &m,
        &ToolResult::success("t", json!("WARNING: disk space low"))
    ));
    assert!(!output_matches(
        &m,
        &ToolResult::success("t", json!("everything fine"))
    ));
}

#[test]
fn output_text_exact() {
    let m = OutputMatcher::Content(ContentMatcher::Text {
        op: MatchOp::Exact,
        value: "done".into(),
    });
    assert!(output_matches(&m, &ToolResult::success("t", json!("done"))));
    assert!(!output_matches(
        &m,
        &ToolResult::success("t", json!("not done"))
    ));
}

// ---------------------------------------------------------------------------
// Output matching — JSON field conditions
// ---------------------------------------------------------------------------

#[test]
fn output_json_fields_match() {
    let m = OutputMatcher::Content(ContentMatcher::JsonFields(vec![FieldCondition {
        path: vec![PathSegment::Field("code".into())],
        op: MatchOp::Exact,
        value: "200".into(),
    }]));
    assert!(output_matches(
        &m,
        &ToolResult::success("t", json!({"code": 200}))
    ));
    assert!(!output_matches(
        &m,
        &ToolResult::success("t", json!({"code": 500}))
    ));
}

#[test]
fn output_json_fields_null_no_match() {
    let m = OutputMatcher::Content(ContentMatcher::JsonFields(vec![FieldCondition {
        path: vec![PathSegment::Field("status".into())],
        op: MatchOp::Exact,
        value: "ok".into(),
    }]));
    assert!(!output_matches(&m, &ToolResult::error("t", "err")));
}

// ---------------------------------------------------------------------------
// Combined input + output matching
// ---------------------------------------------------------------------------

#[test]
fn combined_input_output_match() {
    let pattern = ToolCallPattern {
        tool: ToolMatcher::Exact("Bash".into()),
        args: ArgMatcher::Fields(vec![FieldCondition {
            path: vec![PathSegment::Field("command".into())],
            op: MatchOp::Glob,
            value: "rm *".into(),
        }]),
    };
    let output = OutputMatcher::Status(ToolStatusMatcher::Success);

    let tool_name = "Bash";
    let tool_args = json!({"command": "rm -rf /tmp/test"});
    let tool_result = ToolResult::success("Bash", json!("deleted"));

    // Both match
    assert!(pattern_matches(&pattern, tool_name, &tool_args).is_match());
    assert!(output_matches(&output, &tool_result));
}

#[test]
fn combined_input_matches_but_output_does_not() {
    let pattern = ToolCallPattern::tool("Bash");
    let output = OutputMatcher::Status(ToolStatusMatcher::Error);

    let result = ToolResult::success("Bash", json!("ok"));

    assert!(pattern_matches(&pattern, "Bash", &json!({})).is_match());
    assert!(!output_matches(&output, &result));
}

// ---------------------------------------------------------------------------
// Plugin registration
// ---------------------------------------------------------------------------

use awaken_runtime::plugins::Plugin;

#[test]
fn plugin_has_correct_name() {
    let plugin = ReminderPlugin::new(vec![]);
    assert_eq!(
        plugin.descriptor().name,
        awaken_ext_reminder::plugin::REMINDER_PLUGIN_NAME
    );
}

// ---------------------------------------------------------------------------
// Config loading — new aligned format (tool string pattern)
// ---------------------------------------------------------------------------

#[test]
fn config_loads_multiple_rules() {
    let json = r#"{
        "rules": [
            {
                "tool": "*",
                "output": "any",
                "message": { "target": "system", "content": "A" }
            },
            {
                "tool": "Bash",
                "output": { "status": "error" },
                "message": { "target": "suffix_system", "content": "B" }
            }
        ]
    }"#;

    let config = ReminderRulesConfig::from_str(json, None).unwrap();
    let rules = config.into_rules().unwrap();
    assert_eq!(rules.len(), 2);
    // First rule has auto-generated name
    assert!(rules[0].name.contains("*"));
    // Second rule has auto-generated name with "Bash"
    assert!(rules[1].name.contains("Bash"));
}

#[test]
fn config_invalid_json_returns_error() {
    assert!(ReminderRulesConfig::from_str("{{bad", Some("json")).is_err());
}

#[test]
fn config_with_pattern_args() {
    let json = r#"{
        "rules": [
            {
                "tool": "Bash(command ~ \"rm *\")",
                "output": { "status": "success" },
                "message": { "target": "suffix_system", "content": "deletion detected" }
            }
        ]
    }"#;

    let config = ReminderRulesConfig::from_str(json, None).unwrap();
    let rules = config.into_rules().unwrap();
    assert_eq!(rules.len(), 1);
}

#[test]
fn config_text_glob_content_shorthand() {
    let json = r#"{
        "rules": [
            {
                "tool": "*",
                "output": {
                    "status": "error",
                    "content": "*permission denied*"
                },
                "message": { "target": "suffix_system", "content": "try sudo" }
            }
        ]
    }"#;

    let config = ReminderRulesConfig::from_str(json, None).unwrap();
    let rules = config.into_rules().unwrap();
    assert_eq!(rules.len(), 1);
}

#[test]
fn config_json_fields_content() {
    let json = r#"{
        "rules": [
            {
                "tool": "*",
                "output": {
                    "status": "error",
                    "content": { "fields": [{"path": "error.code", "op": "exact", "value": "403"}] }
                },
                "message": { "target": "suffix_system", "content": "HTTP 403" }
            }
        ]
    }"#;

    let config = ReminderRulesConfig::from_str(json, None).unwrap();
    let rules = config.into_rules().unwrap();
    assert_eq!(rules.len(), 1);
}

// ---------------------------------------------------------------------------
// Multiple rules — all matches fire (not first-match-wins)
// ---------------------------------------------------------------------------

#[test]
fn all_matching_rules_produce_messages() {
    let rules = [
        ReminderRule {
            name: "rule-1".into(),
            pattern: ToolCallPattern::tool_glob("*"),
            output: OutputMatcher::Any,
            message: ContextMessage::system("reminder.rule-1", "Message 1"),
        },
        ReminderRule {
            name: "rule-2".into(),
            pattern: ToolCallPattern::tool("Bash"),
            output: OutputMatcher::Any,
            message: ContextMessage::system("reminder.rule-2", "Message 2"),
        },
        ReminderRule {
            name: "rule-3".into(),
            pattern: ToolCallPattern::tool("Edit"),
            output: OutputMatcher::Any,
            message: ContextMessage::system("reminder.rule-3", "Message 3"),
        },
    ];

    let tool_name = "Bash";
    let tool_args = json!({});
    let tool_result = ToolResult::success("Bash", json!("ok"));

    // Rules 1 and 2 should match, rule 3 should not
    let matched: Vec<&ReminderRule> = rules
        .iter()
        .filter(|rule| {
            pattern_matches(&rule.pattern, tool_name, &tool_args).is_match()
                && output_matches(&rule.output, &tool_result)
        })
        .collect();

    assert_eq!(matched.len(), 2);
    assert_eq!(matched[0].name, "rule-1");
    assert_eq!(matched[1].name, "rule-2");
}

// ---------------------------------------------------------------------------
// Reminder message injection via ContextMessage
// ---------------------------------------------------------------------------

#[test]
fn reminder_produces_correct_context_message() {
    let rule = ReminderRule {
        name: "deletion-warning".into(),
        pattern: ToolCallPattern::tool_with_primary("Bash", "rm *"),
        output: OutputMatcher::Status(ToolStatusMatcher::Success),
        message: ContextMessage::suffix_system(
            "reminder.deletion-warning",
            "Just executed a deletion, verify it was intended",
        )
        .with_cooldown(2),
    };

    assert_eq!(rule.message.key, "reminder.deletion-warning");
    assert_eq!(
        rule.message.target,
        awaken_contract::contract::context_message::ContextMessageTarget::SuffixSystem
    );
    assert_eq!(rule.message.cooldown_turns, 2);
}

#[test]
fn plugin_descriptor_name() {
    let plugin = ReminderPlugin::new(vec![]);
    let desc = plugin.descriptor();
    assert_eq!(desc.name, "reminder");
}
