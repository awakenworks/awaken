use crate::runtime::inference::{LLMResponse, StreamResult};
use crate::runtime::run::{RunAction, TerminationReason};
use crate::runtime::tool_call::gate::{SuspendTicket, ToolCallAction, ToolGate};
use crate::runtime::tool_call::{Suspension, ToolResult};
use crate::runtime::{PendingToolCall, ToolCallResumeMode};
use crate::testing::{mock_tools, test_suspend_ticket, TestFixture};
use crate::thread::ToolCall;
use serde_json::json;

use super::*;

fn test_cm(key: &str, content: &str) -> crate::runtime::inference::ContextMessage {
    crate::runtime::inference::ContextMessage {
        key: key.into(),
        content: content.into(),
        cooldown_turns: 0,
        target: Default::default(),
        consume_after_emit: false,
    }
}

// =========================================================================
// Phase tests
// =========================================================================

#[test]
fn test_phase_display() {
    assert_eq!(Phase::RunStart.to_string(), "RunStart");
    assert_eq!(Phase::StepStart.to_string(), "StepStart");
    assert_eq!(Phase::BeforeInference.to_string(), "BeforeInference");
    assert_eq!(Phase::AfterInference.to_string(), "AfterInference");
    assert_eq!(Phase::BeforeToolExecute.to_string(), "BeforeToolExecute");
    assert_eq!(Phase::AfterToolExecute.to_string(), "AfterToolExecute");
    assert_eq!(Phase::StepEnd.to_string(), "StepEnd");
    assert_eq!(Phase::RunEnd.to_string(), "RunEnd");
}

#[test]
fn test_phase_equality() {
    assert_eq!(Phase::RunStart, Phase::RunStart);
    assert_ne!(Phase::RunStart, Phase::RunEnd);
}

#[test]
fn test_phase_clone() {
    let phase = Phase::BeforeInference;
    let cloned = phase;
    assert_eq!(phase, cloned);
}

#[test]
fn test_phase_policy() {
    let before_inference = Phase::BeforeInference.policy();
    assert!(before_inference.allow_tool_filter_mutation);
    assert!(before_inference.allow_run_action_mutation);
    assert!(!before_inference.allow_tool_gate_mutation);

    let after_inference = Phase::AfterInference.policy();
    assert!(!after_inference.allow_tool_filter_mutation);
    assert!(after_inference.allow_run_action_mutation);
    assert!(!after_inference.allow_tool_gate_mutation);

    let before_tool_execute = Phase::BeforeToolExecute.policy();
    assert!(!before_tool_execute.allow_tool_filter_mutation);
    assert!(!before_tool_execute.allow_run_action_mutation);
    assert!(before_tool_execute.allow_tool_gate_mutation);

    let run_end = Phase::RunEnd.policy();
    assert_eq!(run_end, PhasePolicy::read_only());
}

// =========================================================================
// StepContext tests
// =========================================================================

#[test]
fn test_step_context_new() {
    let fix = TestFixture::new();
    let ctx = fix.step(mock_tools());

    assert!(ctx.inference.context_messages.is_empty());
    assert!(ctx.inference.session_context.is_empty());
    assert_eq!(ctx.inference.tools.len(), 3);
    assert!(ctx.gate.is_none());
    assert!(ctx.llm_response.is_none());
    assert!(ctx.flow.run_action.is_none());
    assert!(ctx.pending_patches.is_empty());
}

#[test]
fn test_step_context_reset() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    ctx.inference.context_messages.push(test_cm("test", "test"));
    ctx.inference.session_context.push("test".into());
    ctx.messaging.reminders.push("test".into());
    ctx.flow.run_action = Some(RunAction::Terminate(TerminationReason::BehaviorRequested));

    ctx.reset();

    assert!(ctx.inference.context_messages.is_empty());
    assert!(ctx.inference.session_context.is_empty());
    assert_eq!(ctx.inference.tools.len(), 3); // tools preserved
    assert!(ctx.messaging.reminders.is_empty() && ctx.messaging.user_messages.is_empty());
    assert!(ctx.flow.run_action.is_none());
    assert!(ctx.pending_patches.is_empty());
}

#[test]
fn test_after_inference_request_termination_sets_run_action() {
    let fix = TestFixture::new();
    let mut step = fix.step(vec![]);
    {
        let mut ctx = AfterInferenceContext::new(&mut step);
        ctx.request_termination(TerminationReason::BehaviorRequested);
    }
    assert!(matches!(
        step.flow.run_action,
        Some(RunAction::Terminate(TerminationReason::BehaviorRequested))
    ));
}

// =========================================================================
// Context injection tests
// =========================================================================

#[test]
fn test_context_messages() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.inference
        .context_messages
        .push(test_cm("k1", "Context 1"));
    ctx.inference
        .context_messages
        .push(test_cm("k2", "Context 2"));

    assert_eq!(ctx.inference.context_messages.len(), 2);
    assert_eq!(ctx.inference.context_messages[0].content, "Context 1");
    assert_eq!(ctx.inference.context_messages[1].content, "Context 2");
}

#[test]
fn test_replace_context_messages() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.inference
        .context_messages
        .push(test_cm("k1", "Context 1"));
    ctx.inference
        .context_messages
        .push(test_cm("k2", "Context 2"));
    ctx.inference.context_messages = vec![test_cm("k3", "Replaced")];

    assert_eq!(ctx.inference.context_messages.len(), 1);
    assert_eq!(ctx.inference.context_messages[0].content, "Replaced");
}

#[test]
fn test_clear_context_messages() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.inference
        .context_messages
        .push(test_cm("k1", "Context 1"));
    ctx.inference.context_messages.clear();

    assert!(ctx.inference.context_messages.is_empty());
}

#[test]
fn test_session_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.inference.session_context.push("Thread 1".into());
    ctx.inference.session_context.push("Thread 2".into());

    assert_eq!(ctx.inference.session_context.len(), 2);
}

#[test]
fn test_set_session_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.inference.session_context.push("Thread 1".into());
    ctx.inference.session_context = vec!["Replaced".to_string()];

    assert_eq!(ctx.inference.session_context.len(), 1);
    assert_eq!(ctx.inference.session_context[0], "Replaced");
}

#[test]
fn test_reminder() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.messaging.reminders.push("Reminder 1".into());
    ctx.messaging.reminders.push("Reminder 2".into());

    assert_eq!(ctx.messaging.reminders.len(), 2);
}

#[test]
fn test_clear_reminders() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.messaging.reminders.push("Reminder 1".into());
    ctx.messaging.reminders.clear();

    assert!(ctx.messaging.reminders.is_empty());
}

// =========================================================================
// Tool filtering tests
// =========================================================================

#[test]
fn test_exclude_tool() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    ctx.inference.tools.retain(|t| t.id != "delete_file");

    assert_eq!(ctx.inference.tools.len(), 2);
    assert!(ctx.inference.tools.iter().all(|t| t.id != "delete_file"));
}

#[test]
fn test_include_only_tools() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    let allowed = ["read_file"];
    ctx.inference
        .tools
        .retain(|t| allowed.contains(&t.id.as_str()));

    assert_eq!(ctx.inference.tools.len(), 1);
    assert_eq!(ctx.inference.tools[0].id, "read_file");
}

// =========================================================================
// Tool control tests (via ToolGate)
// =========================================================================

fn set_tool_gate(ctx: &mut StepContext<'_>, call: &ToolCall) {
    ctx.gate = Some(ToolGate::from_tool_call(call));
}

#[test]
fn test_tool_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "read_file", json!({"path": "/test"}));
    set_tool_gate(&mut ctx, &call);

    assert_eq!(ctx.tool_name(), Some("read_file"));
    assert_eq!(ctx.tool_call_id(), Some("call_1"));
    assert_eq!(ctx.tool_idempotency_key(), Some("call_1"));
    assert_eq!(ctx.tool_args().unwrap()["path"], "/test");
    assert!(!ctx.tool_blocked());
    assert!(!ctx.tool_pending());
}

#[test]
fn test_block_tool() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "delete_file", json!({}));
    set_tool_gate(&mut ctx, &call);

    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = true;
        gate.block_reason = Some("Permission denied".into());
        gate.pending = false;
        gate.suspend_ticket = None;
    }

    assert!(ctx.tool_blocked());
    assert!(!ctx.tool_pending());
    let gate = ctx.gate.as_ref().unwrap();
    assert_eq!(gate.block_reason, Some("Permission denied".to_string()));
    assert!(gate.suspend_ticket.is_none());
}

#[test]
fn test_pending_tool() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "write_file", json!({}));
    set_tool_gate(&mut ctx, &call);

    let interaction = Suspension::new("confirm_1", "confirm").with_message("Allow write?");
    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = false;
        gate.block_reason = None;
        gate.pending = true;
        gate.suspend_ticket = Some(test_suspend_ticket(interaction));
    }

    assert!(ctx.tool_pending());
    assert!(!ctx.tool_blocked());
    let gate = ctx.gate.as_ref().unwrap();
    assert!(gate.block_reason.is_none());
    assert!(gate.suspend_ticket.is_some());
}

#[test]
fn test_confirm_tool() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "write_file", json!({}));
    set_tool_gate(&mut ctx, &call);

    let interaction = Suspension::new("confirm_1", "confirm").with_message("Allow write?");
    if let Some(gate) = ctx.gate.as_mut() {
        gate.pending = true;
        gate.suspend_ticket = Some(test_suspend_ticket(interaction));
    }
    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = false;
        gate.block_reason = None;
        gate.pending = false;
        gate.suspend_ticket = None;
    }

    assert!(!ctx.tool_pending());
    assert!(!ctx.tool_blocked());
    let gate = ctx.gate.as_ref().unwrap();
    assert!(gate.block_reason.is_none());
    assert!(gate.suspend_ticket.is_none());
}

#[test]
fn test_allow_deny_ask_transitions_are_mutually_exclusive() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "write_file", json!({}));
    set_tool_gate(&mut ctx, &call);

    // block
    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = true;
        gate.block_reason = Some("denied".into());
        gate.pending = false;
        gate.suspend_ticket = None;
    }
    assert!(ctx.tool_blocked());
    assert!(!ctx.tool_pending());

    // suspend
    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = false;
        gate.block_reason = None;
        gate.pending = true;
        gate.suspend_ticket = Some(test_suspend_ticket(
            Suspension::new("confirm_1", "confirm").with_message("Allow write?"),
        ));
    }
    assert!(!ctx.tool_blocked());
    assert!(ctx.tool_pending());
    assert!(ctx.gate.as_ref().unwrap().block_reason.is_none());

    // allow
    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = false;
        gate.block_reason = None;
        gate.pending = false;
        gate.suspend_ticket = None;
    }
    assert!(!ctx.tool_blocked());
    assert!(!ctx.tool_pending());
    assert!(ctx.gate.as_ref().unwrap().suspend_ticket.is_none());
}

#[test]
fn test_set_tool_result() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "read_file", json!({}));
    set_tool_gate(&mut ctx, &call);

    let result = ToolResult::success("read_file", json!({"content": "hello"}));
    if let Some(gate) = ctx.gate.as_mut() {
        gate.result = Some(result);
    }

    assert!(ctx.tool_result().is_some());
    assert!(ctx.tool_result().unwrap().is_success());
}

// =========================================================================
// StepOutcome tests
// =========================================================================

#[test]
fn test_step_result_continue() {
    let fix = TestFixture::new();
    let ctx = fix.step(vec![]);
    assert_eq!(ctx.result(), StepOutcome::Continue);
}

#[test]
fn test_step_result_pending() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "write_file", json!({}));
    set_tool_gate(&mut ctx, &call);

    let interaction = Suspension::new("confirm_1", "confirm").with_message("Allow?");
    if let Some(gate) = ctx.gate.as_mut() {
        gate.pending = true;
        gate.suspend_ticket = Some(test_suspend_ticket(interaction.clone()));
    }

    match ctx.result() {
        StepOutcome::Pending(ticket) => assert_eq!(ticket.suspension.id, "confirm_1"),
        _ => panic!("Expected Pending result"),
    }
}

#[test]
fn test_step_result_pending_prefers_suspend_ticket() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "write_file", json!({}));
    set_tool_gate(&mut ctx, &call);

    let ticket_interaction =
        Suspension::new("ticket_1", "confirm").with_message("Suspend via ticket");

    if let Some(gate) = ctx.gate.as_mut() {
        gate.pending = true;
        gate.suspend_ticket = Some(test_suspend_ticket(ticket_interaction.clone()));
    }

    match ctx.result() {
        StepOutcome::Pending(ticket) => {
            assert_eq!(ticket.suspension.id, ticket_interaction.id);
        }
        other => panic!("Expected Pending result, got: {other:?}"),
    }
}

#[test]
fn test_before_tool_execute_decision_prefers_suspend_ticket() {
    let fix = TestFixture::new();
    let mut step = fix.step(vec![]);

    let call = ToolCall::new("call_1", "write_file", json!({}));
    set_tool_gate(&mut step, &call);

    let ticket_interaction =
        Suspension::new("ticket_2", "confirm").with_message("Suspend via ticket");

    if let Some(gate) = step.gate.as_mut() {
        gate.pending = true;
        gate.suspend_ticket = Some(test_suspend_ticket(ticket_interaction.clone()));
    }

    let ctx = BeforeToolExecuteContext::new(&mut step);
    match ctx.decision() {
        ToolCallAction::Suspend(ticket) => {
            assert_eq!(ticket.suspension.id, ticket_interaction.id);
        }
        other => panic!("Expected Suspend decision, got: {other:?}"),
    }
}

#[test]
fn test_step_result_complete() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.llm_response = Some(LLMResponse::success(StreamResult {
        text: "Done!".to_string(),
        tool_calls: vec![],
        usage: None,
        stop_reason: None,
    }));
    assert_eq!(ctx.result(), StepOutcome::Complete);
}

// =========================================================================
// ToolGate tests
// =========================================================================

#[test]
fn test_tool_gate_new() {
    let call = ToolCall::new("call_1", "test_tool", json!({"arg": "value"}));
    let gate = ToolGate::from_tool_call(&call);

    assert_eq!(gate.id, "call_1");
    assert_eq!(gate.idempotency_key(), "call_1");
    assert_eq!(gate.name, "test_tool");
    assert_eq!(gate.args["arg"], "value");
    assert!(gate.result.is_none());
    assert!(!gate.blocked);
    assert!(!gate.pending);
}

#[test]
fn test_tool_gate_is_blocked() {
    let call = ToolCall::new("call_1", "test", json!({}));
    let mut gate = ToolGate::from_tool_call(&call);

    assert!(!gate.is_blocked());
    gate.blocked = true;
    assert!(gate.is_blocked());
}

#[test]
fn test_tool_gate_is_pending() {
    let call = ToolCall::new("call_1", "test", json!({}));
    let mut gate = ToolGate::from_tool_call(&call);

    assert!(!gate.is_pending());
    gate.pending = true;
    assert!(gate.is_pending());
}

// =========================================================================
// Additional edge case tests
// =========================================================================

#[test]
fn test_phase_all_8_values() {
    let phases = [
        Phase::RunStart,
        Phase::StepStart,
        Phase::BeforeInference,
        Phase::AfterInference,
        Phase::BeforeToolExecute,
        Phase::AfterToolExecute,
        Phase::StepEnd,
        Phase::RunEnd,
    ];
    assert_eq!(phases.len(), 8);
    for (i, p1) in phases.iter().enumerate() {
        for (j, p2) in phases.iter().enumerate() {
            if i != j {
                assert_ne!(p1, p2);
            }
        }
    }
}

#[test]
fn test_step_context_empty_session() {
    let fix = TestFixture::new();
    let ctx = fix.step(vec![]);

    assert!(ctx.inference.tools.is_empty());
    assert!(ctx.inference.context_messages.is_empty());
    assert_eq!(ctx.result(), StepOutcome::Continue);
}

#[test]
fn test_step_context_multiple_context_messages() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.inference
        .context_messages
        .push(test_cm("k1", "Context 1"));
    ctx.inference
        .context_messages
        .push(test_cm("k2", "Context 2"));
    ctx.inference
        .context_messages
        .push(test_cm("k3", "Context 3"));

    assert_eq!(ctx.inference.context_messages.len(), 3);
    assert_eq!(ctx.inference.context_messages[0].content, "Context 1");
    assert_eq!(ctx.inference.context_messages[2].content, "Context 3");
}

#[test]
fn test_step_context_multiple_session_contexts() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.inference.session_context.push("Thread 1".into());
    ctx.inference.session_context.push("Thread 2".into());

    assert_eq!(ctx.inference.session_context.len(), 2);
}

#[test]
fn test_step_context_multiple_reminders() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.messaging.reminders.push("Reminder 1".into());
    ctx.messaging.reminders.push("Reminder 2".into());
    ctx.messaging.reminders.push("Reminder 3".into());

    assert_eq!(ctx.messaging.reminders.len(), 3);
}

#[test]
fn test_exclude_nonexistent_tool() {
    let fix = TestFixture::new();
    let tools = mock_tools();
    let original_len = tools.len();
    let mut ctx = fix.step(tools);

    ctx.inference.tools.retain(|t| t.id != "nonexistent_tool");

    assert_eq!(ctx.inference.tools.len(), original_len);
}

#[test]
fn test_exclude_multiple_tools() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    ctx.inference
        .tools
        .retain(|t| t.id != "read_file" && t.id != "delete_file");

    assert_eq!(ctx.inference.tools.len(), 1);
    assert_eq!(ctx.inference.tools[0].id, "write_file");
}

#[test]
fn test_include_only_empty_list() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    let empty: Vec<&str> = vec![];
    ctx.inference
        .tools
        .retain(|t| empty.contains(&t.id.as_str()));

    assert!(ctx.inference.tools.is_empty());
}

#[test]
fn test_include_only_with_nonexistent() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    let allowed = ["read_file", "nonexistent"];
    ctx.inference
        .tools
        .retain(|t| allowed.contains(&t.id.as_str()));

    assert_eq!(ctx.inference.tools.len(), 1);
    assert_eq!(ctx.inference.tools[0].id, "read_file");
}

#[test]
fn test_block_without_tool_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    // No ToolGate, so gate is None — no-op
    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = true;
        gate.block_reason = Some("test".into());
    }
    assert!(!ctx.tool_blocked());
}

#[test]
fn test_pending_without_tool_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let interaction = Suspension::new("id", "confirm").with_message("test");
    if let Some(gate) = ctx.gate.as_mut() {
        gate.pending = true;
        gate.suspend_ticket = Some(test_suspend_ticket(interaction));
    }

    assert!(!ctx.tool_pending());
}

#[test]
fn test_confirm_without_pending() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "test", json!({}));
    set_tool_gate(&mut ctx, &call);
    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = false;
        gate.block_reason = None;
        gate.pending = false;
        gate.suspend_ticket = None;
    }

    assert!(!ctx.tool_pending());
}

#[test]
fn test_tool_args_without_tool() {
    let fix = TestFixture::new();
    let ctx = fix.step(vec![]);
    assert!(ctx.tool_args().is_none());
}

#[test]
fn test_tool_name_without_tool() {
    let fix = TestFixture::new();
    let ctx = fix.step(vec![]);
    assert!(ctx.tool_name().is_none());
    assert!(ctx.tool_call_id().is_none());
    assert!(ctx.tool_idempotency_key().is_none());
}

#[test]
fn test_tool_result_without_tool() {
    let fix = TestFixture::new();
    let ctx = fix.step(vec![]);
    assert!(ctx.tool_result().is_none());
}

#[test]
fn test_step_result_with_tool_calls() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.llm_response = Some(LLMResponse::success(StreamResult {
        text: "Calling tools".to_string(),
        tool_calls: vec![ToolCall::new("call_1", "test", json!({}))],
        usage: None,
        stop_reason: None,
    }));
    assert_eq!(ctx.result(), StepOutcome::Continue);
}

#[test]
fn test_step_result_empty_text_no_tools() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.llm_response = Some(LLMResponse::success(StreamResult {
        text: String::new(),
        tool_calls: vec![],
        usage: None,
        stop_reason: None,
    }));
    assert_eq!(ctx.result(), StepOutcome::Continue);
}

#[test]
fn test_tool_gate_block_reason() {
    let call = ToolCall::new("call_1", "test", json!({}));
    let mut gate = ToolGate::from_tool_call(&call);

    assert!(gate.block_reason.is_none());
    gate.block_reason = Some("Test reason".to_string());
    assert_eq!(gate.block_reason, Some("Test reason".to_string()));
}

#[test]
fn test_tool_gate_suspend_ticket() {
    let call = ToolCall::new("call_1", "test", json!({}));
    let mut gate = ToolGate::from_tool_call(&call);

    assert!(gate.suspend_ticket.is_none());

    let interaction = Suspension::new("confirm_1", "confirm").with_message("Test?");
    gate.suspend_ticket = Some(test_suspend_ticket(interaction.clone()));

    assert_eq!(
        gate.suspend_ticket.as_ref().unwrap().suspension.id,
        "confirm_1"
    );
}

#[test]
fn test_suspend_with_pending_direct() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_copy", "copyToClipboard", json!({"text": "hello"}));
    set_tool_gate(&mut ctx, &call);
    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = true;
        gate.block_reason = Some("old deny state".into());
    }

    let interaction = Suspension::new("call_copy", "tool:copyToClipboard")
        .with_parameters(json!({"text":"hello"}));
    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = false;
        gate.block_reason = None;
        gate.pending = true;
        gate.suspend_ticket = Some(SuspendTicket::new(
            interaction.clone(),
            PendingToolCall::new("call_copy", "copyToClipboard", json!({"text":"hello"})),
            ToolCallResumeMode::UseDecisionAsToolResult,
        ));
    }

    assert!(ctx.tool_pending());
    assert!(!ctx.tool_blocked());
    let gate = ctx.gate.as_ref().unwrap();
    assert!(gate.block_reason.is_none());

    let pending = gate
        .suspend_ticket
        .as_ref()
        .map(|ticket| {
            (
                &ticket.pending,
                ticket.resume_mode,
                ticket.suspension.clone(),
            )
        })
        .expect("pending ticket should exist");
    assert_eq!(pending.0.id, "call_copy");
    assert_eq!(pending.0.name, "copyToClipboard");
    assert_eq!(pending.0.arguments, json!({"text":"hello"}));
    assert_eq!(pending.1, ToolCallResumeMode::UseDecisionAsToolResult);
    assert_eq!(pending.2.id, "call_copy");
    assert_eq!(pending.2.action, "tool:copyToClipboard");
}

#[test]
fn test_suspend_with_pending_replay_tool_call() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_write", "write_file", json!({"path": "a.txt"}));
    set_tool_gate(&mut ctx, &call);

    let call_id = "fc_generated";
    let interaction = Suspension::new(call_id, "tool:PermissionConfirm")
        .with_parameters(json!({"tool_name": "write_file", "tool_args": {"path": "a.txt"}}));
    if let Some(gate) = ctx.gate.as_mut() {
        gate.blocked = false;
        gate.block_reason = None;
        gate.pending = true;
        gate.suspend_ticket = Some(SuspendTicket::new(
            interaction,
            PendingToolCall::new(
                call_id,
                "PermissionConfirm",
                json!({"tool_name": "write_file", "tool_args": {"path": "a.txt"}}),
            ),
            ToolCallResumeMode::ReplayToolCall,
        ));
    }

    assert!(ctx.tool_pending());
    assert!(
        call_id.starts_with("fc_"),
        "expected generated ID, got: {call_id}"
    );
    assert_ne!(call_id, "call_write");

    let gate = ctx.gate.as_ref().unwrap();
    let pending = gate
        .suspend_ticket
        .as_ref()
        .map(|ticket| (&ticket.pending, ticket.resume_mode))
        .expect("pending ticket should exist");
    assert_eq!(pending.0.id, call_id);
    assert_eq!(pending.0.name, "PermissionConfirm");
    assert_eq!(pending.1, ToolCallResumeMode::ReplayToolCall);
}

#[test]
fn test_suspend_pending_without_tool_context_noop() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let interaction =
        Suspension::new("fc_noop", "tool:PermissionConfirm").with_parameters(json!({}));
    // No ToolGate exists, so this is a no-op
    if let Some(gate) = ctx.gate.as_mut() {
        gate.pending = true;
        gate.suspend_ticket = Some(SuspendTicket::new(
            interaction,
            PendingToolCall::new("fc_noop", "PermissionConfirm", json!({})),
            ToolCallResumeMode::UseDecisionAsToolResult,
        ));
    }
    assert!(!ctx.tool_pending());
}

#[test]
fn test_set_clear_session_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.inference.session_context.push("Context 1".into());
    ctx.inference.session_context.push("Context 2".into());
    ctx.inference.session_context = vec!["Only this".to_string()];

    assert_eq!(ctx.inference.session_context.len(), 1);
    assert_eq!(ctx.inference.session_context[0], "Only this");
}
