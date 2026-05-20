//! `EventSink` + `VecEventSink` + a representative cross-section of
//! `AgentEvent` variants (run lifecycle, text/reasoning, tool-call,
//! stream-reset). Pins the streaming surface `reference/events.md` cites.
//! Not exhaustive — the enum has ~20 variants — but covers the families
//! consumers must handle: lifecycle, content, tool, and recovery.

use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::{EventSink, NullEventSink, VecEventSink};
use awaken::contract::lifecycle::TerminationReason;
use awaken::contract::suspension::ToolCallOutcome;
use awaken::contract::tool::ToolResult;
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let sink = VecEventSink::new();

    // Lifecycle: run start + finish.
    sink.emit(AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
        identity: None,
    })
    .await;

    // Content: text + reasoning streaming deltas.
    sink.emit(AgentEvent::TextDelta {
        delta: "hello".into(),
    })
    .await;
    sink.emit(AgentEvent::ReasoningDelta {
        delta: "thinking…".into(),
    })
    .await;

    // Tool-call lifecycle: start → ready → done.
    sink.emit(AgentEvent::ToolCallStart {
        id: "tc-1".into(),
        name: "echo".into(),
    })
    .await;
    sink.emit(AgentEvent::ToolCallReady {
        id: "tc-1".into(),
        name: "echo".into(),
        arguments: json!({ "text": "hi" }),
    })
    .await;
    sink.emit(AgentEvent::ToolCallDone {
        id: "tc-1".into(),
        message_id: "m-1".into(),
        result: ToolResult::success("echo", json!({ "echoed": "hi" })),
        outcome: ToolCallOutcome::Succeeded,
    })
    .await;

    // Stream-recovery: cancel a partial tool call + the wider stream reset.
    sink.emit(AgentEvent::ToolCallCancel {
        id: "tc-2".into(),
        name: "slow_tool".into(),
        reason: "idle stall".into(),
    })
    .await;

    // Finish.
    sink.emit(AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        identity: None,
        result: None,
        termination: TerminationReason::NaturalEnd,
    })
    .await;

    let collected = sink.take();
    // 1 RunStart + 1 TextDelta + 1 ReasoningDelta + 3 tool events
    // (ToolCallStart / Ready / Done) + 1 ToolCallCancel + 1 RunFinish = 8.
    assert_eq!(collected.len(), 8);
    assert!(matches!(collected[0], AgentEvent::RunStart { .. }));
    assert!(matches!(collected[6], AgentEvent::ToolCallCancel { .. }));
    assert!(matches!(collected[7], AgentEvent::RunFinish { .. }));

    // Round-trip a couple variants — wire shape stability.
    let bytes = serde_json::to_vec(&collected[3]).expect("encode tool-start");
    let parsed: AgentEvent = serde_json::from_slice(&bytes).expect("decode tool-start");
    assert!(matches!(parsed, AgentEvent::ToolCallStart { .. }));

    // NullEventSink also satisfies the trait — `run_to_completion` uses it.
    let null = NullEventSink;
    null.emit(AgentEvent::TextDelta {
        delta: "discarded".into(),
    })
    .await;
}
