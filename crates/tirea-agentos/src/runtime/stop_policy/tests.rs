use super::conditions::*;
use super::plugin::{condition_from_spec, derive_stats_from_messages, derive_stats_from_messages_with_response};
use super::state::{StopPolicyRuntimeAction, StopPolicyRuntimeState};
use super::StopPolicyPlugin;

use crate::composition::StopConditionSpec;
use crate::contracts::runtime::tool_call::ToolResult;
use crate::contracts::thread::{Message, ToolCall};
use crate::contracts::{RunContext, StreamResult};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use tirea_state::LatticeRegistry;

#[test]
fn derives_round_stats_from_messages() {
    let call_1 = ToolCall::new("c1", "failing", json!({}));
    let call_2 = ToolCall::new("c2", "echo", json!({}));
    let messages = vec![
        Arc::new(Message::assistant_with_tool_calls(
            "r1",
            vec![call_1.clone()],
        )),
        Arc::new(Message::tool(
            "c1",
            serde_json::to_string(&ToolResult::error("failing", "boom")).unwrap(),
        )),
        Arc::new(Message::assistant_with_tool_calls(
            "r2",
            vec![call_2.clone()],
        )),
        Arc::new(Message::tool(
            "c2",
            serde_json::to_string(&ToolResult::success("echo", json!({"ok": true}))).unwrap(),
        )),
    ];
    let stats = derive_stats_from_messages(&messages);
    assert_eq!(stats.step, 2);
    assert_eq!(stats.total_tool_call_count, 2);
    assert_eq!(stats.step_tool_call_count, 1);
    assert_eq!(stats.last_tool_calls.len(), 1);
    assert_eq!(stats.last_tool_calls[0].id, call_2.id);
    assert_eq!(stats.last_tool_calls[0].name, call_2.name);
    assert_eq!(stats.last_text, "r2");
    assert_eq!(stats.consecutive_errors, 0);
}

#[test]
fn derives_stats_with_current_response() {
    let prior_messages = vec![Arc::new(Message::user("u1"))];
    let response = StreamResult {
        text: "r1".to_string(),
        tool_calls: vec![ToolCall::new("c1", "echo", json!({}))],
        usage: None,
        stop_reason: None,
    };
    let stats = derive_stats_from_messages_with_response(&prior_messages, &response);
    assert_eq!(stats.step, 1);
    assert_eq!(stats.step_tool_call_count, 1);
    assert_eq!(stats.total_tool_call_count, 1);
    assert_eq!(stats.last_text, "r1");
    assert_eq!(stats.last_tool_calls.len(), 1);
    assert_eq!(stats.last_tool_calls[0].id, "c1");
}

/// Empty run (no prior messages, text-only response with no tools).
#[test]
fn stats_text_only_response_counts_step_but_no_tools() {
    let messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("hello"))];
    let response = StreamResult {
        text: "hi there".to_string(),
        tool_calls: vec![],
        usage: None,
        stop_reason: None,
    };
    let stats = derive_stats_from_messages_with_response(&messages, &response);
    assert_eq!(stats.step, 1, "text-only response still counts as a step");
    assert_eq!(stats.step_tool_call_count, 0);
    assert_eq!(stats.total_tool_call_count, 0);
    assert_eq!(stats.last_text, "hi there");
    assert!(stats.tool_call_history.is_empty());
}

/// No messages at all -- response is the only thing.
#[test]
fn stats_empty_messages_with_response() {
    let messages: Vec<Arc<Message>> = vec![];
    let response = StreamResult {
        text: "hi".to_string(),
        tool_calls: vec![],
        usage: None,
        stop_reason: None,
    };
    let stats = derive_stats_from_messages_with_response(&messages, &response);
    assert_eq!(stats.step, 1);
    assert_eq!(stats.total_tool_call_count, 0);
    assert_eq!(stats.consecutive_errors, 0);
}

/// Consecutive errors reset to 0 when a round has no tool calls.
#[test]
fn consecutive_errors_reset_on_no_tool_round() {
    let fail_call = ToolCall::new("f1", "broken", json!({}));
    let messages = vec![
        // Round 1: tool fails
        Arc::new(Message::assistant_with_tool_calls(
            "r1",
            vec![fail_call.clone()],
        )),
        Arc::new(Message::tool(
            &fail_call.id,
            serde_json::to_string(&ToolResult::error("broken", "boom")).unwrap(),
        )),
        // Round 2: text-only (no tools) -- resets consecutive errors
        Arc::new(Message::assistant("text only")),
    ];
    let stats = derive_stats_from_messages(&messages);
    assert_eq!(
        stats.consecutive_errors, 0,
        "text-only round resets consecutive errors"
    );
    assert_eq!(stats.step, 2);
}

/// tool_call_history caps at 20 entries (sliding window).
#[test]
fn tool_call_history_caps_at_twenty() {
    let mut messages: Vec<Arc<Message>> = Vec::new();
    for i in 0..25 {
        let call = ToolCall::new(&format!("c{i}"), &format!("tool_{i}"), json!({}));
        messages.push(Arc::new(Message::assistant_with_tool_calls(
            format!("r{i}"),
            vec![call.clone()],
        )));
        messages.push(Arc::new(Message::tool(
            &call.id,
            serde_json::to_string(&ToolResult::success(
                &format!("tool_{i}"),
                json!({"ok": true}),
            ))
            .unwrap(),
        )));
    }
    let stats = derive_stats_from_messages(&messages);
    assert_eq!(stats.step, 25);
    assert_eq!(
        stats.tool_call_history.len(),
        20,
        "history must be capped at 20"
    );
    // Oldest entries are dropped -- first entry should be tool_5 (index 5).
    assert_eq!(stats.tool_call_history[0], vec!["tool_5".to_string()]);
}

/// Multiple tool calls in one round are sorted and tracked together.
#[test]
fn multi_tool_round_sorted_in_history() {
    let calls = vec![
        ToolCall::new("c1", "zebra", json!({})),
        ToolCall::new("c2", "alpha", json!({})),
    ];
    let messages = vec![
        Arc::new(Message::assistant_with_tool_calls("r1", calls.clone())),
        Arc::new(Message::tool(
            "c1",
            serde_json::to_string(&ToolResult::success("zebra", json!({}))).unwrap(),
        )),
        Arc::new(Message::tool(
            "c2",
            serde_json::to_string(&ToolResult::success("alpha", json!({}))).unwrap(),
        )),
    ];
    let stats = derive_stats_from_messages(&messages);
    assert_eq!(stats.step, 1);
    assert_eq!(stats.step_tool_call_count, 2);
    assert_eq!(stats.total_tool_call_count, 2);
    assert_eq!(
        stats.tool_call_history[0],
        vec!["alpha".to_string(), "zebra".to_string()],
        "tool names must be sorted within a round"
    );
}

/// Partial tool failure: only when ALL tools fail does consecutive_errors increment.
#[test]
fn consecutive_errors_only_on_all_tools_failing() {
    let ok_call = ToolCall::new("c1", "ok_tool", json!({}));
    let fail_call = ToolCall::new("c2", "bad_tool", json!({}));
    let messages = vec![
        Arc::new(Message::assistant_with_tool_calls(
            "r1",
            vec![ok_call.clone(), fail_call.clone()],
        )),
        Arc::new(Message::tool(
            &ok_call.id,
            serde_json::to_string(&ToolResult::success("ok_tool", json!({}))).unwrap(),
        )),
        Arc::new(Message::tool(
            &fail_call.id,
            serde_json::to_string(&ToolResult::error("bad_tool", "fail")).unwrap(),
        )),
    ];
    let stats = derive_stats_from_messages(&messages);
    assert_eq!(
        stats.consecutive_errors, 0,
        "mixed success/fail does not count as all-error"
    );
}

#[test]
fn stop_condition_spec_serialization_roundtrip() {
    let specs = vec![
        StopConditionSpec::MaxRounds { rounds: 5 },
        StopConditionSpec::Timeout { seconds: 30 },
        StopConditionSpec::TokenBudget { max_total: 1000 },
        StopConditionSpec::ConsecutiveErrors { max: 3 },
        StopConditionSpec::StopOnTool {
            tool_name: "finish".to_string(),
        },
        StopConditionSpec::ContentMatch {
            pattern: "DONE".to_string(),
        },
        StopConditionSpec::LoopDetection { window: 4 },
    ];
    for spec in specs {
        let encoded = serde_json::to_string(&spec).unwrap();
        let restored: StopConditionSpec = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored, spec);
    }
}

#[test]
fn stop_policy_registers_lattice_paths() {
    use crate::contracts::runtime::behavior::AgentBehavior;

    let mut registry = LatticeRegistry::new();
    let plugin = StopPolicyPlugin::new(vec![], vec![]);
    plugin.register_lattice_paths(&mut registry);
    assert!(
        registry
            .get(&tirea_state::parse_path(
                "__kernel.stop_policy_runtime.total_input_tokens"
            ))
            .is_some(),
        "total_input_tokens should be registered"
    );
    assert!(
        registry
            .get(&tirea_state::parse_path(
                "__kernel.stop_policy_runtime.total_output_tokens"
            ))
            .is_some(),
        "total_output_tokens should be registered"
    );
}

#[test]
fn record_tokens_increments_gcounters() {
    let mut state = StopPolicyRuntimeState::default();
    assert_eq!(state.total_input_tokens.value(), 0);
    assert_eq!(state.total_output_tokens.value(), 0);

    state.reduce(StopPolicyRuntimeAction::RecordTokens {
        started_at_ms: Some(1000),
        prompt_tokens: 100,
        completion_tokens: 50,
    });
    assert_eq!(state.total_input_tokens.value(), 100);
    assert_eq!(state.total_output_tokens.value(), 50);
    assert_eq!(state.started_at_ms, Some(1000));

    state.reduce(StopPolicyRuntimeAction::RecordTokens {
        started_at_ms: Some(2000),
        prompt_tokens: 200,
        completion_tokens: 150,
    });
    assert_eq!(state.total_input_tokens.value(), 300);
    assert_eq!(state.total_output_tokens.value(), 200);
    assert_eq!(
        state.started_at_ms,
        Some(1000),
        "started_at_ms should not change once set"
    );
}

/// Regression: `derive_stats_from_messages` used to count assistant messages
/// from the entire thread history, not just the current run. When a thread is
/// reused across multiple runs, the step counter accumulated across runs,
/// causing MaxRounds to fire prematurely on subsequent runs.
#[test]
fn stats_should_only_count_current_run_messages() {
    // Simulate a thread with 5 prior assistant turns from previous runs.
    let mut prior_messages: Vec<Arc<Message>> = Vec::new();
    for i in 0..5 {
        let call = ToolCall::new(&format!("old-{i}"), "echo", json!({}));
        prior_messages.push(Arc::new(Message::user(format!("u{i}"))));
        prior_messages.push(Arc::new(Message::assistant_with_tool_calls(
            format!("prior-{i}"),
            vec![call.clone()],
        )));
        prior_messages.push(Arc::new(Message::tool(
            &call.id,
            serde_json::to_string(&ToolResult::success("echo", json!({"ok": true}))).unwrap(),
        )));
    }

    // Current run adds a new user message.
    let run_start = prior_messages.len();
    prior_messages.push(Arc::new(Message::user("new-user-message")));

    // Current LLM response -- this is the 1st assistant turn of the NEW run.
    let response = StreamResult {
        text: "new-response".to_string(),
        tool_calls: vec![ToolCall::new("new-1", "echo", json!({}))],
        usage: None,
        stop_reason: None,
    };

    // Only count messages from run_start onward.
    let stats =
        derive_stats_from_messages_with_response(&prior_messages[run_start..], &response);
    assert_eq!(
        stats.step, 1,
        "step must be 1 (only the current run's assistant turn), not 6"
    );
    assert_eq!(stats.total_tool_call_count, 1);
    assert_eq!(stats.consecutive_errors, 0);
}

/// Prior run's consecutive tool errors must not carry over into a new run.
#[test]
fn consecutive_errors_do_not_leak_across_runs() {
    // Prior run: 3 rounds all failing.
    let mut messages: Vec<Arc<Message>> = Vec::new();
    for i in 0..3 {
        let call = ToolCall::new(&format!("fail-{i}"), "broken", json!({}));
        messages.push(Arc::new(Message::user(format!("u{i}"))));
        messages.push(Arc::new(Message::assistant_with_tool_calls(
            format!("a{i}"),
            vec![call.clone()],
        )));
        messages.push(Arc::new(Message::tool(
            &call.id,
            serde_json::to_string(&ToolResult::error("broken", "boom")).unwrap(),
        )));
    }

    // New run boundary.
    let run_start = messages.len();
    messages.push(Arc::new(Message::user("new-turn")));

    // New run: first response succeeds.
    let response = StreamResult {
        text: "ok".to_string(),
        tool_calls: vec![ToolCall::new("ok-1", "echo", json!({}))],
        usage: None,
        stop_reason: None,
    };

    let stats = derive_stats_from_messages_with_response(&messages[run_start..], &response);
    assert_eq!(
        stats.consecutive_errors, 0,
        "errors from prior run must not leak"
    );
    assert_eq!(stats.step, 1);
}

/// Prior run's tool call history must not trigger loop detection in a new run.
#[test]
fn tool_call_history_does_not_leak_across_runs() {
    // Prior run: 3 rounds calling the same tool "echo".
    let mut messages: Vec<Arc<Message>> = Vec::new();
    for i in 0..3 {
        let call = ToolCall::new(&format!("old-{i}"), "echo", json!({}));
        messages.push(Arc::new(Message::user(format!("u{i}"))));
        messages.push(Arc::new(Message::assistant_with_tool_calls(
            format!("a{i}"),
            vec![call.clone()],
        )));
        messages.push(Arc::new(Message::tool(
            &call.id,
            serde_json::to_string(&ToolResult::success("echo", json!({"ok": true}))).unwrap(),
        )));
    }

    let run_start = messages.len();
    messages.push(Arc::new(Message::user("new-turn")));

    // New run: first response also calls "echo" -- should NOT trigger loop.
    let response = StreamResult {
        text: "".to_string(),
        tool_calls: vec![ToolCall::new("new-1", "echo", json!({}))],
        usage: None,
        stop_reason: None,
    };

    let stats = derive_stats_from_messages_with_response(&messages[run_start..], &response);
    assert_eq!(
        stats.tool_call_history.len(),
        1,
        "only 1 round in the new run"
    );
}

/// Prior run's total_tool_call_count must not accumulate into the new run.
#[test]
fn total_tool_call_count_does_not_leak_across_runs() {
    let mut messages: Vec<Arc<Message>> = Vec::new();
    for i in 0..4 {
        let call = ToolCall::new(&format!("old-{i}"), "echo", json!({}));
        messages.push(Arc::new(Message::user(format!("u{i}"))));
        messages.push(Arc::new(Message::assistant_with_tool_calls(
            format!("a{i}"),
            vec![call.clone()],
        )));
        messages.push(Arc::new(Message::tool(
            &call.id,
            serde_json::to_string(&ToolResult::success("echo", json!({"ok": true}))).unwrap(),
        )));
    }

    let run_start = messages.len();
    messages.push(Arc::new(Message::user("new-turn")));

    let response = StreamResult {
        text: "".to_string(),
        tool_calls: vec![
            ToolCall::new("n1", "a", json!({})),
            ToolCall::new("n2", "b", json!({})),
        ],
        usage: None,
        stop_reason: None,
    };

    let stats = derive_stats_from_messages_with_response(&messages[run_start..], &response);
    assert_eq!(
        stats.total_tool_call_count, 2,
        "only 2 tool calls in the new run, not 6"
    );
}

/// Fresh thread (run_start=0): all messages belong to the current run.
#[test]
fn fresh_thread_counts_all_messages() {
    let messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("hello"))];
    let run_start = 0;

    let response = StreamResult {
        text: "hi".to_string(),
        tool_calls: vec![ToolCall::new("c1", "echo", json!({}))],
        usage: None,
        stop_reason: None,
    };

    let stats = derive_stats_from_messages_with_response(&messages[run_start..], &response);
    assert_eq!(stats.step, 1);
    assert_eq!(stats.total_tool_call_count, 1);
}

// -----------------------------------------------------------------------
// Built-in policy evaluate tests
// -----------------------------------------------------------------------

/// Construct a `StopPolicyInput` with defaults and evaluate the given policy.
macro_rules! eval_policy {
    ($policy:expr, { $($field:ident : $val:expr),* $(,)? }) => {{
        let run_ctx = RunContext::new(
            "t", json!({}), vec![], crate::contracts::RunPolicy::default(),
        );
        let empty_history = VecDeque::new();
        let no_tools: &[ToolCall] = &[];
        #[allow(unused_mut, unused_assignments)]
        let mut stats = StopPolicyStats {
            step: 0,
            step_tool_call_count: 0,
            total_tool_call_count: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            consecutive_errors: 0,
            elapsed: std::time::Duration::ZERO,
            last_tool_calls: no_tools,
            last_text: "",
            tool_call_history: &empty_history,
        };
        $(stats.$field = $val;)*
        let input = StopPolicyInput { run_ctx: &run_ctx, stats };
        $policy.evaluate(&input)
    }};
}

#[test]
fn max_rounds_does_not_fire_below_limit() {
    assert!(eval_policy!(MaxRounds(5), { step: 4 }).is_none());
}

#[test]
fn max_rounds_fires_at_limit() {
    let r = eval_policy!(MaxRounds(5), { step: 5 }).unwrap();
    assert_eq!(r.code, "max_rounds_reached");
}

#[test]
fn max_rounds_fires_above_limit() {
    assert!(eval_policy!(MaxRounds(5), { step: 10 }).is_some());
}

#[test]
fn max_rounds_step_zero_does_not_fire() {
    assert!(eval_policy!(MaxRounds(5), {}).is_none());
}

#[test]
fn consecutive_errors_fires_at_threshold() {
    let r = eval_policy!(ConsecutiveErrors(3), { consecutive_errors: 3 }).unwrap();
    assert_eq!(r.code, "consecutive_errors_exceeded");
}

#[test]
fn consecutive_errors_does_not_fire_below_threshold() {
    assert!(eval_policy!(ConsecutiveErrors(3), { consecutive_errors: 2 }).is_none());
}

#[test]
fn consecutive_errors_zero_max_never_fires() {
    assert!(
        eval_policy!(ConsecutiveErrors(0), { consecutive_errors: 100 }).is_none(),
        "max=0 means disabled"
    );
}

#[test]
fn loop_detection_fires_on_repeated_tool_pattern() {
    let mut h = VecDeque::new();
    h.push_back(vec!["read".into(), "write".into()]);
    h.push_back(vec!["read".into(), "write".into()]);
    let r = eval_policy!(LoopDetection { window: 3 }, { tool_call_history: &h }).unwrap();
    assert_eq!(r.code, "loop_detected");
}

#[test]
fn loop_detection_does_not_fire_on_different_patterns() {
    let mut h = VecDeque::new();
    h.push_back(vec!["read".into()]);
    h.push_back(vec!["write".into()]);
    assert!(eval_policy!(LoopDetection { window: 3 }, { tool_call_history: &h }).is_none());
}

#[test]
fn loop_detection_needs_at_least_two_rounds() {
    let mut h = VecDeque::new();
    h.push_back(vec!["read".into()]);
    assert!(
        eval_policy!(LoopDetection { window: 2 }, { tool_call_history: &h }).is_none(),
        "single round cannot form a loop"
    );
}

#[test]
fn loop_detection_window_clamps_to_minimum_two() {
    let mut h = VecDeque::new();
    h.push_back(vec!["x".into()]);
    h.push_back(vec!["x".into()]);
    assert!(
        eval_policy!(LoopDetection { window: 1 }, { tool_call_history: &h }).is_some(),
        "window clamped to 2 still detects the pair"
    );
}

#[test]
fn content_match_empty_pattern_never_fires() {
    assert!(
        eval_policy!(ContentMatch(String::new()), { last_text: "anything here" }).is_none(),
        "empty pattern must not match"
    );
}

#[test]
fn content_match_fires_on_substring() {
    let r =
        eval_policy!(ContentMatch("DONE".into()), { last_text: "work is DONE now" }).unwrap();
    assert_eq!(r.code, "content_matched");
    assert_eq!(r.detail.as_deref(), Some("DONE"));
}

#[test]
fn content_match_no_match() {
    assert!(
        eval_policy!(ContentMatch("DONE".into()), { last_text: "still working" }).is_none()
    );
}

#[test]
fn token_budget_zero_never_fires() {
    assert!(
        eval_policy!(TokenBudget { max_total: 0 }, {
            total_input_tokens: 999_999, total_output_tokens: 999_999
        })
        .is_none(),
        "max_total=0 means unlimited"
    );
}

#[test]
fn token_budget_fires_at_limit() {
    let r = eval_policy!(TokenBudget { max_total: 1000 }, {
        total_input_tokens: 600, total_output_tokens: 400
    })
    .unwrap();
    assert_eq!(r.code, "token_budget_exceeded");
}

#[test]
fn token_budget_does_not_fire_below_limit() {
    assert!(eval_policy!(TokenBudget { max_total: 1000 }, {
        total_input_tokens: 400, total_output_tokens: 500
    })
    .is_none());
}

#[test]
fn stop_on_tool_no_match() {
    let calls = [ToolCall::new("c1", "echo", json!({}))];
    assert!(eval_policy!(StopOnTool("finish".into()), { last_tool_calls: &calls }).is_none());
}

#[test]
fn stopped_reason_payload() {
    let calls = [ToolCall::new("c1", "finish", json!({}))];
    let r = eval_policy!(StopOnTool("finish".into()), {
        step: 1,
        step_tool_call_count: 1,
        total_tool_call_count: 1,
        last_tool_calls: &calls
    })
    .unwrap();
    assert_eq!(r.code, "tool_called");
    assert_eq!(r.detail.as_deref(), Some("finish"));
}

// -----------------------------------------------------------------------
// condition_from_spec factory
// -----------------------------------------------------------------------

#[test]
fn condition_from_spec_max_rounds() {
    let c = condition_from_spec(StopConditionSpec::MaxRounds { rounds: 7 });
    assert_eq!(c.id(), "max_rounds");
    assert!(eval_policy!(&*c, { step: 7 }).is_some());
}

#[test]
fn condition_from_spec_timeout() {
    let c = condition_from_spec(StopConditionSpec::Timeout { seconds: 10 });
    assert_eq!(c.id(), "timeout");
    assert!(eval_policy!(&*c, { elapsed: std::time::Duration::from_secs(10) }).is_some());
}

#[test]
fn condition_from_spec_token_budget() {
    let c = condition_from_spec(StopConditionSpec::TokenBudget { max_total: 500 });
    assert_eq!(c.id(), "token_budget");
    assert!(eval_policy!(&*c, { total_input_tokens: 300, total_output_tokens: 200 }).is_some());
}

#[test]
fn condition_from_spec_consecutive_errors() {
    let c = condition_from_spec(StopConditionSpec::ConsecutiveErrors { max: 2 });
    assert_eq!(c.id(), "consecutive_errors");
    assert!(eval_policy!(&*c, { consecutive_errors: 2 }).is_some());
}

#[test]
fn condition_from_spec_stop_on_tool() {
    let c = condition_from_spec(StopConditionSpec::StopOnTool {
        tool_name: "done".into(),
    });
    assert_eq!(c.id(), "stop_on_tool");
    let calls = [ToolCall::new("c1", "done", json!({}))];
    assert!(eval_policy!(&*c, { last_tool_calls: &calls }).is_some());
}

#[test]
fn condition_from_spec_content_match() {
    let c = condition_from_spec(StopConditionSpec::ContentMatch {
        pattern: "END".into(),
    });
    assert_eq!(c.id(), "content_match");
    assert!(eval_policy!(&*c, { last_text: "THE END" }).is_some());
}

#[test]
fn condition_from_spec_loop_detection() {
    let c = condition_from_spec(StopConditionSpec::LoopDetection { window: 3 });
    assert_eq!(c.id(), "loop_detection");
    let mut h = VecDeque::new();
    h.push_back(vec!["x".to_string()]);
    h.push_back(vec!["x".to_string()]);
    assert!(eval_policy!(&*c, { tool_call_history: &h }).is_some());
}
