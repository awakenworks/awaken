//! Cross-protocol parity matrix (P1-#9).
//!
//! The per-protocol encoders each have deep unit suites (`ai-sdk`, `ag-ui`,
//! `a2a`, `managed`), but nothing pins the *agreement between* wires: that one
//! neutral [`StepOutcome`] projects to a coherent terminal on every front door,
//! that the same assistant text and tool-call id surface on each, and that an
//! internal audit fact leaks onto none. This file is that matrix — the analogue
//! of awaken-next's `protocol_parity.rs`, but at the encoder seam so it is
//! deterministic (the HTTP cross-protocol interop tests race on the live
//! projection and are `#[ignore]`d).
//!
//! All four adapters consume the *same* `StepOutcome` (or the neutral facts it
//! folds to), so any divergence is a real semantic disagreement, not a fixture
//! drift. Divergences that are intended (A2A maps budget-exhaustion to `failed`;
//! managed carries a fault out-of-band on `session.error`) are asserted
//! explicitly so a future edit cannot silently change them.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::event::{Fact, Transcoder, fold_messages};
use awaken_protocol_transport::{Pending, StepFailure, StepOutcome, Terminal};

use awaken_protocol_a2a::encoder as a2a;
use awaken_protocol_a2a::types::TaskState;
use awaken_protocol_ag_ui::encoder as agui;
use awaken_protocol_ag_ui::types::AgUiEvent;
use awaken_protocol_ai_sdk::encoder as ai;
use awaken_protocol_ai_sdk::types::UIStreamEvent;
use awaken_protocol_managed::project::{ManagedEncoder, ProjectedEvent};
use awaken_protocol_managed::types::{OutboundKind, StopReason};

// --- shared drivers: one StepOutcome, four wires -------------------------

/// The AI-SDK UI-message stream for a committed step.
fn ai_events(outcome: &StepOutcome) -> Vec<UIStreamEvent> {
    ai::encode_step(outcome)
}

/// The AG-UI event stream for a committed step.
fn agui_events(outcome: &StepOutcome) -> Vec<AgUiEvent> {
    agui::encode_step(outcome, "t1", "r1")
}

/// The managed `OutboundKind`s for a committed step. Managed drops `RunStarted`
/// and has no single `encode_step(outcome)` (its `project_step` takes a
/// `Terminus`, which cannot express `Failed`), so drive the transcoder over the
/// same neutral facts the other wires fold from `outcome`.
fn managed_events(outcome: &StepOutcome) -> Vec<OutboundKind> {
    let pending = outcome
        .pending()
        .map(|p| (p.tool_use_id.as_str(), p.client_executed));
    let mut facts = fold_messages(&outcome.new_messages, pending);
    facts.push(outcome.terminal_event());
    ManagedEncoder::default()
        .transcode_facts(&facts)
        .into_iter()
        .map(|ProjectedEvent { kind, .. }| kind)
        .collect()
}

/// The A2A terminal `TaskState` for a committed step (history = the step's
/// committed messages, as the real router passes the post-step transcript).
fn a2a_state(outcome: &StepOutcome) -> TaskState {
    a2a::encode_task("t1", &outcome.new_messages, outcome)
        .status
        .state
}

// --- terminal classifiers per wire ---------------------------------------

/// The AI-SDK `finishReason`, and whether an `error` frame was emitted.
fn ai_terminal(events: &[UIStreamEvent]) -> (Option<String>, bool) {
    let reason = events.iter().find_map(|e| match e {
        UIStreamEvent::Finish { finish_reason, .. } => Some(finish_reason.clone()),
        _ => None,
    });
    let errored = events
        .iter()
        .any(|e| matches!(e, UIStreamEvent::Error { .. }));
    (reason.flatten(), errored)
}

/// The AG-UI terminal: `"finished"`, `"error"`, or `"none"`.
fn agui_terminal(events: &[AgUiEvent]) -> &'static str {
    let finished = events
        .iter()
        .any(|e| matches!(e, AgUiEvent::RunFinished { .. }));
    let errored = events
        .iter()
        .any(|e| matches!(e, AgUiEvent::RunError { .. }));
    match (finished, errored) {
        (true, false) => "finished",
        (false, true) => "error",
        (false, false) => "none",
        (true, true) => "both",
    }
}

/// The managed idle stop reason (the single terminal frame).
fn managed_stop(events: &[OutboundKind]) -> StopReason {
    events
        .iter()
        .find_map(|e| match e {
            OutboundKind::SessionStatusIdle { stop_reason } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("managed step ends with exactly one session.status_idle")
}

// --- fixtures ------------------------------------------------------------

fn user(id: &str, text: &str) -> Message {
    Message::text(Id(id.into()), Role::User, text)
}
fn assistant(id: &str, text: &str) -> Message {
    Message::text(Id(id.into()), Role::Assistant, text)
}
fn assistant_tool(id: &str, call: &str, name: &str, input: serde_json::Value) -> Message {
    Message {
        id: Id(id.into()),
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: call.into(),
            name: name.into(),
            input,
        }],
    }
}
fn tool_result(id: &str, call: &str, text: &str) -> Message {
    Message {
        id: Id(id.into()),
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: call.into(),
            content: vec![ContentBlock::text(text)],
        }],
    }
}

fn finished(msgs: Vec<Message>) -> StepOutcome {
    StepOutcome {
        new_messages: msgs,
        terminal: Terminal::Finished,
    }
}

// =========================================================================
// 1. Terminal parity — one StepOutcome, the four wires must agree per the
//    documented mapping table.
// =========================================================================

#[test]
fn a_natural_finish_is_a_clean_terminal_on_every_wire() {
    let outcome = finished(vec![assistant("a1", "done")]);
    let ai = ai_events(&outcome);
    let (reason, errored) = ai_terminal(&ai);
    assert_eq!(reason.as_deref(), Some("stop"), "ai-sdk: {ai:?}");
    assert!(!errored, "ai-sdk finish is not an error: {ai:?}");
    assert_eq!(agui_terminal(&agui_events(&outcome)), "finished");
    assert_eq!(a2a_state(&outcome), TaskState::Completed);
    assert_eq!(managed_stop(&managed_events(&outcome)), StopReason::EndTurn);
}

#[test]
fn parking_on_a_tool_is_action_required_on_every_wire() {
    let outcome = StepOutcome {
        new_messages: vec![assistant_tool(
            "a1",
            "c1",
            "submit_answer",
            serde_json::json!({}),
        )],
        terminal: Terminal::Waiting {
            pending: Some(Pending {
                tool_use_id: "c1".into(),
                name: "submit_answer".into(),
                input: serde_json::json!({}),
                client_executed: true,
            }),
        },
    };
    let (reason, _) = ai_terminal(&ai_events(&outcome));
    assert_eq!(reason.as_deref(), Some("tool-calls"));
    // AG-UI has no dedicated interrupt frame; a park closes with RUN_FINISHED.
    assert_eq!(agui_terminal(&agui_events(&outcome)), "finished");
    assert_eq!(a2a_state(&outcome), TaskState::InputRequired);
    match managed_stop(&managed_events(&outcome)) {
        StopReason::RequiresAction { event_ids } => assert_eq!(event_ids, vec!["c1".to_string()]),
        other => panic!("managed parks on RequiresAction, got {other:?}"),
    }
}

#[test]
fn a_failure_surfaces_as_an_error_wherever_the_wire_can_and_never_as_success() {
    let outcome = StepOutcome {
        new_messages: vec![assistant("a1", "partial")],
        terminal: Terminal::Failed(StepFailure {
            code: "inference_failed".into(),
            message: "upstream is down".into(),
        }),
    };
    // AI-SDK: an `error` frame plus finish("error") — never finish("stop").
    let ai = ai_events(&outcome);
    let (reason, errored) = ai_terminal(&ai);
    assert_eq!(reason.as_deref(), Some("error"), "ai-sdk: {ai:?}");
    assert!(errored, "ai-sdk emits an error frame: {ai:?}");
    // AG-UI: RUN_ERROR, and NOT also RUN_FINISHED.
    assert_eq!(agui_terminal(&agui_events(&outcome)), "error");
    // A2A: a failed task.
    assert_eq!(a2a_state(&outcome), TaskState::Failed);
    // Managed has no error stop reason: a fault idles the session with EndTurn and
    // carries the fault out-of-band on `session.error`. Pin that documented
    // divergence so nobody "fixes" it into a fake success or a bogus stop reason.
    assert_eq!(managed_stop(&managed_events(&outcome)), StopReason::EndTurn);
}

#[test]
fn budget_exhaustion_is_a_finish_for_streaming_wires_but_failed_for_a2a() {
    // The one intended cross-wire divergence: exhaustion is a *clean* terminus on
    // the streaming wires (no "exhausted" finish reason exists), a distinct
    // `RetriesExhausted` on managed, and — because A2A has only completed/failed —
    // a `failed` task. Pins all four so a refactor can't quietly realign them.
    let outcome = StepOutcome {
        new_messages: vec![assistant("a1", "ran out")],
        terminal: Terminal::Exhausted,
    };
    let ai = ai_events(&outcome);
    let (reason, errored) = ai_terminal(&ai);
    assert_eq!(reason.as_deref(), Some("stop"), "ai-sdk: {ai:?}");
    assert!(!errored, "exhaustion is not an ai-sdk error: {ai:?}");
    assert_eq!(agui_terminal(&agui_events(&outcome)), "finished");
    assert_eq!(a2a_state(&outcome), TaskState::Failed);
    assert_eq!(
        managed_stop(&managed_events(&outcome)),
        StopReason::RetriesExhausted
    );
}

// =========================================================================
// 2. Content parity — the same transcript surfaces the same facts on each wire.
// =========================================================================

#[test]
fn assistant_text_is_visible_on_every_wire() {
    let outcome = finished(vec![user("u1", "hi"), assistant("a1", "hello there")]);

    let ai = ai_events(&outcome);
    assert!(
        ai.iter()
            .any(|e| matches!(e, UIStreamEvent::TextDelta { delta, .. } if delta == "hello there")),
        "ai-sdk carries the assistant text: {ai:?}"
    );

    let agui = agui_events(&outcome);
    assert!(
        agui.iter().any(
            |e| matches!(e, AgUiEvent::TextMessageContent { delta, .. } if delta == "hello there")
        ),
        "ag-ui carries the assistant text: {agui:?}"
    );

    let managed = managed_events(&outcome);
    assert!(
        managed.iter().any(|e| matches!(
            e,
            OutboundKind::AgentMessage { content }
                if content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "hello there"))
        )),
        "managed carries the assistant text: {managed:?}"
    );

    // A2A folds the transcript into its Task history + status message.
    let task = a2a::encode_task("t1", &outcome.new_messages, &outcome);
    assert_eq!(
        task.status.message.expect("status message").text(),
        "hello there"
    );
    assert!(task.history.iter().any(|m| m.text() == "hello there"));
}

#[test]
fn a_tool_call_correlation_id_survives_on_the_streaming_wires() {
    // A completed server tool: the call id `c1` must be correlatable on every
    // streaming wire. (A2A is a user/agent-text snapshot and deliberately drops
    // tool/system messages — asserted separately below.)
    let outcome = finished(vec![
        assistant_tool("a1", "c1", "read", serde_json::json!({ "path": "x" })),
        tool_result("t1", "c1", "42"),
    ]);

    let ai = ai_events(&outcome);
    assert!(
        ai.iter().any(|e| matches!(
            e,
            UIStreamEvent::ToolInputAvailable { tool_call_id, .. } if tool_call_id == "c1"
        )),
        "ai-sdk tool-input keyed by c1: {ai:?}"
    );
    assert!(
        ai.iter().any(|e| matches!(
            e,
            UIStreamEvent::ToolOutputAvailable { tool_call_id, .. } if tool_call_id == "c1"
        )),
        "ai-sdk tool-output keyed by c1: {ai:?}"
    );

    let agui = agui_events(&outcome);
    assert!(
        agui.iter().any(|e| matches!(
            e,
            AgUiEvent::ToolCallStart { tool_call_id, .. } if tool_call_id == "c1"
        )),
        "ag-ui tool-call-start keyed by c1: {agui:?}"
    );

    // Managed keys the tool-use ProjectedEvent by the call id itself.
    let pending = None;
    let mut facts = fold_messages(&outcome.new_messages, pending);
    facts.push(outcome.terminal_event());
    let managed = ManagedEncoder::default().transcode_facts(&facts);
    assert!(
        managed.iter().any(|pe| pe.id.as_deref() == Some("c1")
            && matches!(pe.kind, OutboundKind::AgentToolUse { .. })),
        "managed tool-use carries id c1"
    );
}

#[test]
fn a2a_snapshot_drops_tool_and_system_messages_by_design() {
    // Pins the documented A2A boundary: only user/agent text becomes history, so a
    // tool-only step yields an empty history (no leakage of internal tool traffic).
    let outcome = finished(vec![
        assistant_tool("a1", "c1", "read", serde_json::json!({ "path": "x" })),
        tool_result("t1", "c1", "42"),
    ]);
    let task = a2a::encode_task("t1", &outcome.new_messages, &outcome);
    assert!(
        task.history.is_empty(),
        "A2A history omits tool/system messages: {:?}",
        task.history
    );
    assert_eq!(task.status.state, TaskState::Completed);
}

// =========================================================================
// 3. Suppression / ordering — the terminal is the last frame on each stream
//    wire; nothing content-bearing follows it.
// =========================================================================

#[test]
fn the_terminal_frame_is_last_on_every_streaming_wire() {
    let outcome = finished(vec![
        assistant("a1", "here you go"),
        assistant_tool("a2", "c1", "read", serde_json::json!({ "path": "x" })),
        tool_result("t1", "c1", "ok"),
    ]);

    let ai = ai_events(&outcome);
    assert!(
        matches!(ai.last(), Some(UIStreamEvent::Finish { .. })),
        "ai-sdk ends on finish: {ai:?}"
    );
    // No content part appears after the terminal finish.
    let finish_at = ai
        .iter()
        .position(|e| matches!(e, UIStreamEvent::Finish { .. }))
        .unwrap();
    assert!(
        ai.get(finish_at + 1).is_none(),
        "nothing follows the ai-sdk finish: {ai:?}"
    );

    let agui = agui_events(&outcome);
    assert!(
        matches!(agui.last(), Some(AgUiEvent::RunFinished { .. })),
        "ag-ui ends on RUN_FINISHED: {agui:?}"
    );

    let managed = managed_events(&outcome);
    assert!(
        matches!(managed.last(), Some(OutboundKind::SessionStatusIdle { .. })),
        "managed ends on session.status_idle: {managed:?}"
    );
    // Exactly one terminal frame per wire (no double-finish).
    assert_eq!(
        managed
            .iter()
            .filter(|e| matches!(e, OutboundKind::SessionStatusIdle { .. }))
            .count(),
        1,
        "managed emits exactly one idle: {managed:?}"
    );
    assert_eq!(
        agui.iter()
            .filter(|e| matches!(
                e,
                AgUiEvent::RunFinished { .. } | AgUiEvent::RunError { .. }
            ))
            .count(),
        1,
        "ag-ui emits exactly one terminal: {agui:?}"
    );
    assert_eq!(
        ai.iter()
            .filter(|e| matches!(e, UIStreamEvent::Finish { .. }))
            .count(),
        1,
        "ai-sdk emits exactly one finish: {ai:?}"
    );
}

// =========================================================================
// 4. Internal audit facts leak onto no wire.
// =========================================================================

#[test]
fn a_continuation_guard_round_leaks_onto_no_wire() {
    // `Fact::Continuation` is an audit lifecycle fact (`classify().live == false`);
    // every streaming transcoder must drop it. Drive each directly so the omission
    // is pinned uniformly, not per-encoder.
    use awaken_protocol_ag_ui::encoder::AgUiEncoder;
    use awaken_protocol_ai_sdk::encoder::AiSdkEncoder;

    let cont = Fact::Continuation {
        steered: true,
        detail: serde_json::json!({ "reason": "auto_continue" }),
    };
    assert!(
        AiSdkEncoder::new().fact(&cont).is_empty(),
        "ai-sdk drops continuation"
    );
    assert!(
        AgUiEncoder::new("t1", "r1").fact(&cont).is_empty(),
        "ag-ui drops continuation"
    );
    assert!(
        ManagedEncoder::default().fact(&cont).is_empty(),
        "managed drops continuation"
    );
}

#[test]
fn run_started_opens_the_streaming_wires_but_is_silent_on_managed() {
    // The lifecycle-start fact brackets the AI-SDK/AG-UI streams (start / RUN_STARTED)
    // but managed has no per-step start event, so it drops it. Parity here is
    // "each wire takes its documented stance", not "identical output".
    use awaken_protocol_ag_ui::encoder::AgUiEncoder;
    use awaken_protocol_ai_sdk::encoder::AiSdkEncoder;

    assert_eq!(
        AiSdkEncoder::new().fact(&Fact::RunStarted),
        vec![UIStreamEvent::Start, UIStreamEvent::StartStep]
    );
    assert!(matches!(
        AgUiEncoder::new("t1", "r1")
            .fact(&Fact::RunStarted)
            .as_slice(),
        [AgUiEvent::RunStarted { .. }]
    ));
    assert!(
        ManagedEncoder::default().fact(&Fact::RunStarted).is_empty(),
        "managed has no per-step start event"
    );
}
