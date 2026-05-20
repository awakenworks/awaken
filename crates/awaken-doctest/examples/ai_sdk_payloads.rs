//! `UIStreamEvent` frame shapes that the AI SDK v6 client consumes —
//! pins the wire format `reference/protocols/ai-sdk-v6.md` cites for
//! `/v1/ai-sdk/chat`. Covers a representative cross-section of the frame
//! families: message lifecycle, text streaming, tool input/output, and
//! step boundaries. Not exhaustive — the enum has ~25 variants.

use awaken::server::protocols::ai_sdk_v6::types::UIStreamEvent;
use serde_json::json;

fn main() {
    // Message lifecycle — `type: "start"` per AI SDK v6 wire spec.
    let start = UIStreamEvent::MessageStart {
        message_id: Some("msg-1".into()),
        message_metadata: None,
    };
    let json_start = serde_json::to_value(&start).expect("encode start");
    assert_eq!(json_start["type"], "start");
    assert_eq!(json_start["messageId"], "msg-1");

    // Text streaming: start → delta → end.
    let text_delta = UIStreamEvent::TextDelta {
        id: "block-1".into(),
        delta: "hello".into(),
        provider_metadata: None,
    };
    let dj = serde_json::to_value(&text_delta).expect("encode text-delta");
    assert_eq!(dj["type"], "text-delta");
    assert_eq!(dj["delta"], "hello");

    // Round-trip — runtime emits bytes, client parses them.
    let parsed: UIStreamEvent = serde_json::from_value(dj).expect("decode");
    assert_eq!(parsed, text_delta);

    // Tool input lifecycle — `useChat()` renders these as a pending call.
    let tool_input_start = UIStreamEvent::ToolInputStart {
        tool_call_id: "tc-1".into(),
        tool_name: "echo".into(),
        dynamic: None,
    };
    let tj = serde_json::to_value(&tool_input_start).expect("encode tool-input-start");
    assert_eq!(tj["type"], "tool-input-start");
    assert_eq!(tj["toolCallId"], "tc-1");

    let tool_input_avail = UIStreamEvent::ToolInputAvailable {
        tool_call_id: "tc-1".into(),
        tool_name: "echo".into(),
        input: json!({ "text": "hi" }),
        dynamic: None,
    };
    let tia = serde_json::to_value(&tool_input_avail).expect("encode tool-input-available");
    assert_eq!(tia["type"], "tool-input-available");
    assert_eq!(tia["input"]["text"], "hi");

    // Tool output — the matching result frame `useChat()` pairs by toolCallId.
    let tool_output = UIStreamEvent::ToolOutputAvailable {
        tool_call_id: "tc-1".into(),
        output: json!({ "echoed": "hi" }),
        dynamic: None,
        preliminary: None,
        provider_executed: None,
    };
    let to = serde_json::to_value(&tool_output).expect("encode tool-output-available");
    assert_eq!(to["type"], "tool-output-available");

    // Step boundaries — multi-step agent runs emit StartStep / FinishStep.
    let start_step = serde_json::to_value(UIStreamEvent::StartStep).expect("encode start-step");
    assert_eq!(start_step["type"], "start-step");

    // Finish frame closes the response stream.
    let finish = UIStreamEvent::Finish {
        finish_reason: Some("stop".into()),
        message_metadata: None,
    };
    let fj = serde_json::to_value(&finish).expect("encode finish");
    assert_eq!(fj["type"], "finish");
    assert_eq!(fj["finishReason"], "stop");
}
