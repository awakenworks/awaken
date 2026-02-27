use super::*;
use crate::runtime::llm::StreamResult;
use crate::runtime::run::TerminationReason;
use crate::runtime::tool_call::{Suspension, ToolResult};
use crate::runtime::{PendingToolCall, ToolCallResumeMode};
use crate::testing::{mock_tools, test_suspend_ticket, TestFixture};
use crate::thread::ToolCall;
use serde_json::json;

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

    assert!(ctx.system_context.is_empty());
    assert!(ctx.session_context.is_empty());
    assert!(ctx.system_reminders.is_empty());
    assert_eq!(ctx.tools.len(), 3);
    assert!(ctx.tool.is_none());
    assert!(ctx.response.is_none());
    assert!(ctx.run_action.is_none());
    assert!(ctx.pending_patches.is_empty());
}

#[test]
fn test_step_context_reset() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    ctx.system("test");
    ctx.thread("test");
    ctx.reminder("test");
    ctx.set_run_action(RunAction::Terminate(TerminationReason::BehaviorRequested));

    ctx.reset();

    assert!(ctx.system_context.is_empty());
    assert!(ctx.session_context.is_empty());
    assert!(ctx.system_reminders.is_empty());
    assert!(ctx.run_action.is_none());
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
    assert_eq!(
        step.run_action,
        Some(RunAction::Terminate(TerminationReason::BehaviorRequested))
    );
}

// =========================================================================
// Context injection tests
// =========================================================================

#[test]
fn test_system_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.system("Context 1");
    ctx.system("Context 2");

    assert_eq!(ctx.system_context.len(), 2);
    assert_eq!(ctx.system_context[0], "Context 1");
    assert_eq!(ctx.system_context[1], "Context 2");
}

#[test]
fn test_set_system_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.system("Context 1");
    ctx.system("Context 2");
    ctx.system_context = vec!["Replaced".to_string()];

    assert_eq!(ctx.system_context.len(), 1);
    assert_eq!(ctx.system_context[0], "Replaced");
}

#[test]
fn test_clear_system_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.system("Context 1");
    ctx.system_context.clear();

    assert!(ctx.system_context.is_empty());
}

#[test]
fn test_session_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.thread("Thread 1");
    ctx.thread("Thread 2");

    assert_eq!(ctx.session_context.len(), 2);
}

#[test]
fn test_set_session_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.thread("Thread 1");
    ctx.session_context = vec!["Replaced".to_string()];

    assert_eq!(ctx.session_context.len(), 1);
    assert_eq!(ctx.session_context[0], "Replaced");
}

#[test]
fn test_reminder() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.reminder("Reminder 1");
    ctx.reminder("Reminder 2");

    assert_eq!(ctx.system_reminders.len(), 2);
}

#[test]
fn test_clear_reminders() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.reminder("Reminder 1");
    ctx.system_reminders.clear();

    assert!(ctx.system_reminders.is_empty());
}

// =========================================================================
// Tool filtering tests
// =========================================================================

#[test]
fn test_exclude_tool() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    ctx.exclude("delete_file");

    assert_eq!(ctx.tools.len(), 2);
    assert!(ctx.tools.iter().all(|t| t.id != "delete_file"));
}

#[test]
fn test_include_only_tools() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    ctx.include_only(&["read_file"]);

    assert_eq!(ctx.tools.len(), 1);
    assert_eq!(ctx.tools[0].id, "read_file");
}

// =========================================================================
// Tool control tests
// =========================================================================

#[test]
fn test_tool_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "read_file", json!({"path": "/test"}));
    ctx.tool = Some(ToolContext::new(&call));

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
    ctx.tool = Some(ToolContext::new(&call));

    ctx.block("Permission denied");

    assert!(ctx.tool_blocked());
    assert!(!ctx.tool_pending());
    assert_eq!(
        ctx.tool.as_ref().unwrap().block_reason,
        Some("Permission denied".to_string())
    );
    assert!(ctx.tool.as_ref().unwrap().suspend_ticket.is_none());
}

#[test]
fn test_pending_tool() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "write_file", json!({}));
    ctx.tool = Some(ToolContext::new(&call));

    let interaction = Suspension::new("confirm_1", "confirm").with_message("Allow write?");
    ctx.suspend(test_suspend_ticket(interaction));

    assert!(ctx.tool_pending());
    assert!(!ctx.tool_blocked());
    assert!(ctx.tool.as_ref().unwrap().block_reason.is_none());
    assert!(ctx.tool.as_ref().unwrap().suspend_ticket.is_some());
}

#[test]
fn test_confirm_tool() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "write_file", json!({}));
    ctx.tool = Some(ToolContext::new(&call));

    let interaction = Suspension::new("confirm_1", "confirm").with_message("Allow write?");
    ctx.suspend(test_suspend_ticket(interaction));
    ctx.allow();

    assert!(!ctx.tool_pending());
    assert!(!ctx.tool_blocked());
    assert!(ctx.tool.as_ref().unwrap().block_reason.is_none());
    assert!(ctx.tool.as_ref().unwrap().suspend_ticket.is_none());
}

#[test]
fn test_allow_deny_ask_transitions_are_mutually_exclusive() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "write_file", json!({}));
    ctx.tool = Some(ToolContext::new(&call));

    ctx.block("denied");
    assert!(ctx.tool_blocked());
    assert!(!ctx.tool_pending());

    ctx.suspend(test_suspend_ticket(
        Suspension::new("confirm_1", "confirm").with_message("Allow write?"),
    ));
    assert!(!ctx.tool_blocked());
    assert!(ctx.tool_pending());
    assert!(ctx.tool.as_ref().unwrap().block_reason.is_none());

    ctx.allow();
    assert!(!ctx.tool_blocked());
    assert!(!ctx.tool_pending());
    assert!(ctx.tool.as_ref().unwrap().suspend_ticket.is_none());
}

#[test]
fn test_set_tool_result() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "read_file", json!({}));
    ctx.tool = Some(ToolContext::new(&call));

    let result = ToolResult::success("read_file", json!({"content": "hello"}));
    ctx.set_tool_result(result);

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
    ctx.tool = Some(ToolContext::new(&call));

    let interaction = Suspension::new("confirm_1", "confirm").with_message("Allow?");
    ctx.suspend(test_suspend_ticket(interaction.clone()));

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
    ctx.tool = Some(ToolContext::new(&call));

    let ticket_interaction =
        Suspension::new("ticket_1", "confirm").with_message("Suspend via ticket");

    if let Some(tool) = ctx.tool.as_mut() {
        tool.pending = true;
        tool.suspend_ticket = Some(test_suspend_ticket(ticket_interaction.clone()));
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
    step.tool = Some(ToolContext::new(&call));

    let ticket_interaction =
        Suspension::new("ticket_2", "confirm").with_message("Suspend via ticket");

    if let Some(tool) = step.tool.as_mut() {
        tool.pending = true;
        tool.suspend_ticket = Some(test_suspend_ticket(ticket_interaction.clone()));
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

    ctx.response = Some(StreamResult {
        text: "Done!".to_string(),
        tool_calls: vec![],
        usage: None,
    });

    assert_eq!(ctx.result(), StepOutcome::Complete);
}

// =========================================================================
// ToolContext tests
// =========================================================================

#[test]
fn test_tool_context_new() {
    let call = ToolCall::new("call_1", "test_tool", json!({"arg": "value"}));
    let tool_ctx = ToolContext::new(&call);

    assert_eq!(tool_ctx.id, "call_1");
    assert_eq!(tool_ctx.idempotency_key(), "call_1");
    assert_eq!(tool_ctx.name, "test_tool");
    assert_eq!(tool_ctx.args["arg"], "value");
    assert!(tool_ctx.result.is_none());
    assert!(!tool_ctx.blocked);
    assert!(!tool_ctx.pending);
}

#[test]
fn test_tool_context_is_blocked() {
    let call = ToolCall::new("call_1", "test", json!({}));
    let mut tool_ctx = ToolContext::new(&call);

    assert!(!tool_ctx.is_blocked());
    tool_ctx.blocked = true;
    assert!(tool_ctx.is_blocked());
}

#[test]
fn test_tool_context_is_pending() {
    let call = ToolCall::new("call_1", "test", json!({}));
    let mut tool_ctx = ToolContext::new(&call);

    assert!(!tool_ctx.is_pending());
    tool_ctx.pending = true;
    assert!(tool_ctx.is_pending());
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
    // All should be unique
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

    assert!(ctx.tools.is_empty());
    assert!(ctx.system_context.is_empty());
    assert_eq!(ctx.result(), StepOutcome::Continue);
}

#[test]
fn test_step_context_multiple_system_contexts() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.system("Context 1");
    ctx.system("Context 2");
    ctx.system("Context 3");

    assert_eq!(ctx.system_context.len(), 3);
    assert_eq!(ctx.system_context[0], "Context 1");
    assert_eq!(ctx.system_context[2], "Context 3");
}

#[test]
fn test_step_context_multiple_session_contexts() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.thread("Thread 1");
    ctx.thread("Thread 2");

    assert_eq!(ctx.session_context.len(), 2);
}

#[test]
fn test_step_context_multiple_reminders() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.reminder("Reminder 1");
    ctx.reminder("Reminder 2");
    ctx.reminder("Reminder 3");

    assert_eq!(ctx.system_reminders.len(), 3);
}

#[test]
fn test_exclude_nonexistent_tool() {
    let fix = TestFixture::new();
    let tools = mock_tools();
    let original_len = tools.len();
    let mut ctx = fix.step(tools);

    ctx.exclude("nonexistent_tool");

    // Should not change anything
    assert_eq!(ctx.tools.len(), original_len);
}

#[test]
fn test_exclude_multiple_tools() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    ctx.exclude("read_file");
    ctx.exclude("delete_file");

    assert_eq!(ctx.tools.len(), 1);
    assert_eq!(ctx.tools[0].id, "write_file");
}

#[test]
fn test_include_only_empty_list() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    ctx.include_only(&[]);

    assert!(ctx.tools.is_empty());
}

#[test]
fn test_include_only_with_nonexistent() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(mock_tools());

    ctx.include_only(&["read_file", "nonexistent"]);

    assert_eq!(ctx.tools.len(), 1);
    assert_eq!(ctx.tools[0].id, "read_file");
}

#[test]
fn test_block_without_tool_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    // No tool context, block should not panic
    ctx.block("test");

    assert!(!ctx.tool_blocked()); // tool_blocked returns false when no tool
}

#[test]
fn test_pending_without_tool_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let interaction = Suspension::new("id", "confirm").with_message("test");
    ctx.suspend(test_suspend_ticket(interaction));

    assert!(!ctx.tool_pending()); // tool_pending returns false when no tool
}

#[test]
fn test_confirm_without_pending() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_1", "test", json!({}));
    ctx.tool = Some(ToolContext::new(&call));

    // Confirm without pending should not panic
    ctx.allow();

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

    ctx.response = Some(StreamResult {
        text: "Calling tools".to_string(),
        tool_calls: vec![ToolCall::new("call_1", "test", json!({}))],
        usage: None,
    });

    // With tool calls, should continue
    assert_eq!(ctx.result(), StepOutcome::Continue);
}

#[test]
fn test_step_result_empty_text_no_tools() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.response = Some(StreamResult {
        text: String::new(),
        tool_calls: vec![],
        usage: None,
    });

    // Empty text, no tool calls -> Continue (not Complete)
    assert_eq!(ctx.result(), StepOutcome::Continue);
}

#[test]
fn test_tool_context_block_reason() {
    let call = ToolCall::new("call_1", "test", json!({}));
    let mut tool_ctx = ToolContext::new(&call);

    assert!(tool_ctx.block_reason.is_none());
    tool_ctx.block_reason = Some("Test reason".to_string());
    assert_eq!(tool_ctx.block_reason, Some("Test reason".to_string()));
}

#[test]
fn test_tool_context_suspend_ticket() {
    let call = ToolCall::new("call_1", "test", json!({}));
    let mut tool_ctx = ToolContext::new(&call);

    assert!(tool_ctx.suspend_ticket.is_none());

    let interaction = Suspension::new("confirm_1", "confirm").with_message("Test?");
    tool_ctx.suspend_ticket = Some(test_suspend_ticket(interaction.clone()));

    assert_eq!(
        tool_ctx.suspend_ticket.as_ref().unwrap().suspension.id,
        "confirm_1"
    );
}

#[test]
fn test_suspend_with_pending_direct() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    let call = ToolCall::new("call_copy", "copyToClipboard", json!({"text": "hello"}));
    ctx.tool = Some(ToolContext::new(&call));
    ctx.block("old deny state");

    let interaction = Suspension::new("call_copy", "tool:copyToClipboard")
        .with_parameters(json!({"text":"hello"}));
    ctx.suspend(SuspendTicket::new(
        interaction.clone(),
        PendingToolCall::new("call_copy", "copyToClipboard", json!({"text":"hello"})),
        ToolCallResumeMode::UseDecisionAsToolResult,
    ));

    assert!(ctx.tool_pending());
    assert!(!ctx.tool_blocked());
    assert!(ctx.tool.as_ref().unwrap().block_reason.is_none());

    let pending = ctx
        .tool
        .as_ref()
        .unwrap()
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
    ctx.tool = Some(ToolContext::new(&call));

    let call_id = "fc_generated";
    let interaction = Suspension::new(call_id, "tool:PermissionConfirm")
        .with_parameters(json!({"tool_name": "write_file", "tool_args": {"path": "a.txt"}}));
    ctx.suspend(SuspendTicket::new(
        interaction,
        PendingToolCall::new(
            call_id,
            "PermissionConfirm",
            json!({"tool_name": "write_file", "tool_args": {"path": "a.txt"}}),
        ),
        ToolCallResumeMode::ReplayToolCall,
    ));

    assert!(ctx.tool_pending());
    // ReplayOriginalTool should use a dedicated frontend call id.
    assert!(
        call_id.starts_with("fc_"),
        "expected generated ID, got: {call_id}"
    );
    assert_ne!(call_id, "call_write");

    let pending = ctx
        .tool
        .as_ref()
        .unwrap()
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
    ctx.suspend(SuspendTicket::new(
        interaction,
        PendingToolCall::new("fc_noop", "PermissionConfirm", json!({})),
        ToolCallResumeMode::UseDecisionAsToolResult,
    ));
    assert!(!ctx.tool_pending());
}

#[test]
fn test_set_clear_session_context() {
    let fix = TestFixture::new();
    let mut ctx = fix.step(vec![]);

    ctx.thread("Context 1");
    ctx.thread("Context 2");
    ctx.session_context = vec!["Only this".to_string()];

    assert_eq!(ctx.session_context.len(), 1);
    assert_eq!(ctx.session_context[0], "Only this");
}
