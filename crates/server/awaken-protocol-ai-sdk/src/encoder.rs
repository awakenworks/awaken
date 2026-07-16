//! Project a committed step (and thread history) into AI SDK UI Message Stream
//! parts. The step fold is shared (`awaken_agent_contract::event`); this module
//! owns the AI SDK *transcoder* — the `Fact -> UIStreamEvent` mapping — and
//! the history read-model fold.

use std::collections::HashSet;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::event::{
    Delta, Fact, HistorySink, ToolDisposition, ToolUseRef, Transcoder, fold_history, fold_messages,
};
use awaken_protocol_transport::{StepOutcome, blocks_text};
use serde_json::Value;

use crate::types::{UIStreamEvent, history_message, text_parts};

/// The AI SDK v6 transcoder: the one per-protocol adapter for both tiers (ADR-0058
/// Axis 9). `fact()` projects committed whole-units (`tool-input-available`,
/// `finish`); `delta()` projects live increments (`text-*`, `tool-input-delta`) as
/// the run streams. A pending tool (client- or built-in) is *not* provider-executed,
/// so the client renders its interaction; a step boundary becomes `start`/`finish`.
/// State (`open_text`, `text_seq`, `tools`) is the live-prefix bookkeeping; committed
/// projection via a fresh default is stateless.
#[derive(Default)]
pub struct AiSdkEncoder {
    /// `start`/`start-step` already emitted (idempotent on a repeated `RunStarted`).
    started: bool,
    /// At least one live increment flowed — the router uses this to pick the
    /// committed tail (`encode_close`) over the full projection (`encode_step`).
    streamed: bool,
    /// The id of the open live text block, if a text run is currently streaming.
    open_text: Option<String>,
    text_seq: usize,
    /// Call ids that already emitted `tool-input-start` live.
    tools: HashSet<String>,
}

impl AiSdkEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once any live increment has been emitted, so the router knows to append
    /// the committed *tail* rather than the full committed projection.
    pub fn has_streamed(&self) -> bool {
        self.streamed
    }

    /// Close any open live text run at stream end (before the committed tail).
    pub fn finalize(&mut self) -> Vec<UIStreamEvent> {
        self.close_text()
    }

    fn close_text(&mut self) -> Vec<UIStreamEvent> {
        match self.open_text.take() {
            Some(id) => vec![UIStreamEvent::TextEnd { id }],
            None => Vec::new(),
        }
    }
}

impl Transcoder for AiSdkEncoder {
    type Output = UIStreamEvent;

    fn fact(&mut self, event: &Fact) -> Vec<UIStreamEvent> {
        match event {
            Fact::RunStarted => {
                if self.started {
                    Vec::new()
                } else {
                    self.started = true;
                    vec![UIStreamEvent::Start, UIStreamEvent::StartStep]
                }
            }
            Fact::AssistantMessage { id, content } => {
                let text = blocks_text(content);
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![
                        UIStreamEvent::TextStart { id: id.clone() },
                        UIStreamEvent::TextDelta {
                            id: id.clone(),
                            delta: text,
                        },
                        UIStreamEvent::TextEnd { id: id.clone() },
                    ]
                }
            }
            Fact::ToolCall {
                id,
                name,
                input,
                disposition,
            } => vec![UIStreamEvent::ToolInputAvailable {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                input: input.clone(),
                provider_executed: matches!(disposition, ToolDisposition::Executed),
            }],
            Fact::ToolResult {
                id,
                content,
                is_error,
            } => {
                if *is_error {
                    vec![UIStreamEvent::ToolOutputError {
                        tool_call_id: id.clone(),
                        error_text: blocks_text(content),
                    }]
                } else {
                    vec![UIStreamEvent::ToolOutputAvailable {
                        tool_call_id: id.clone(),
                        output: parse_output(content),
                    }]
                }
            }
            Fact::Waiting { .. } => {
                vec![
                    UIStreamEvent::FinishStep,
                    UIStreamEvent::finish("tool-calls"),
                ]
            }
            Fact::RunFinished { .. } => {
                vec![UIStreamEvent::FinishStep, UIStreamEvent::finish("stop")]
            }
            Fact::RunFailed { code, message } => vec![
                UIStreamEvent::error(format!("{code}: {message}")),
                UIStreamEvent::FinishStep,
                UIStreamEvent::finish("error"),
            ],
            // An internal continuation-guard round is not an AI-SDK wire part; the
            // committed fold never emits it into this stream.
            Fact::Continuation { .. } => Vec::new(),
        }
    }

    fn delta(&mut self, delta: &Delta) -> Vec<UIStreamEvent> {
        self.streamed = true;
        match delta {
            Delta::TextDelta { delta } => {
                let mut out = Vec::new();
                let id = match &self.open_text {
                    Some(id) => id.clone(),
                    None => {
                        let id = format!("txt-{}", self.text_seq);
                        self.text_seq += 1;
                        self.open_text = Some(id.clone());
                        out.push(UIStreamEvent::TextStart { id: id.clone() });
                        id
                    }
                };
                out.push(UIStreamEvent::TextDelta {
                    id,
                    delta: delta.clone(),
                });
                out
            }
            Delta::ToolCallDelta {
                id,
                name,
                args_delta,
            } => {
                let mut out = self.close_text();
                if self.tools.insert(id.clone()) {
                    out.push(UIStreamEvent::ToolInputStart {
                        tool_call_id: id.clone(),
                        tool_name: name.clone(),
                    });
                }
                // `args_delta` is already the de-accumulated suffix; the committed
                // `tool-input-available` carries the parsed input (from `fact`).
                if !args_delta.is_empty() {
                    out.push(UIStreamEvent::ToolInputDelta {
                        tool_call_id: id.clone(),
                        input_text_delta: args_delta.clone(),
                    });
                }
                out
            }
            // Reasoning is not projected to the AI SDK live prefix (opt-in tier).
            Delta::ReasoningDelta { .. } => Vec::new(),
        }
    }
}

/// Project one committed step into an ordered UI Message Stream. Each response is a
/// self-contained stream: `start` … `finish`.
pub fn encode_step(outcome: &StepOutcome) -> Vec<UIStreamEvent> {
    let pending = outcome
        .pending()
        .map(|p| (p.tool_use_id.as_str(), p.client_executed));
    let mut events = vec![Fact::RunStarted];
    events.extend(fold_messages(&outcome.new_messages, pending));
    // The terminal event owns the failed / parked / finished distinction — a fault
    // becomes `RunFailed`, which transcodes to `error` + `finish("error")`.
    events.push(outcome.terminal_event());
    AiSdkEncoder::new().transcode_facts(&events)
}

/// Project the *authoritative tail* of a committed step, for a turn whose
/// in-flight prefix — `start`/`start-step`, streamed `text-*`, and
/// `tool-input-start`/`tool-input-delta` — was already emitted live (see
/// the encoder's live `delta()`). Drops `RunStarted` (already `start`ed) and
/// assistant text (already streamed as deltas); keeps the parsed authoritative
/// `tool-input-available`, any tool output, and the `finish` frames. The live
/// prefix plus this tail form one well-formed UI Message Stream.
pub fn encode_close(outcome: &StepOutcome) -> Vec<UIStreamEvent> {
    let pending = outcome
        .pending()
        .map(|p| (p.tool_use_id.as_str(), p.client_executed));
    let mut events = fold_messages(&outcome.new_messages, pending);
    // Same terminal event as `encode_step` (a fault closes with `error` +
    // `finish("error")`); the live prefix already carried `start`/`start-step`.
    events.push(outcome.terminal_event());
    // The live channel already carried `start`/`start-step` and every text delta;
    // emitting them again would double the stream. Keep only tool + finish frames.
    events.retain(|e| !matches!(e, Fact::AssistantMessage { .. }));
    AiSdkEncoder::new().transcode_facts(&events)
}

/// Parse a tool result's text as JSON, falling back to a string.
fn parse_output(content: &[ContentBlock]) -> Value {
    let text = blocks_text(content);
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

/// Fold committed thread messages into AI SDK `UIMessage`s for the history
/// endpoint. Assistant tool calls merge with their later tool result into a single
/// `output-available` part (`providerExecuted: true`). This is a read-model fold,
/// distinct from the streaming projection above; the shared walk lives in
/// [`fold_history`], this sink only shapes each message the AI SDK way.
pub fn encode_history(messages: &[Message]) -> Vec<Value> {
    let mut sink = AiSdkHistorySink::default();
    fold_history(messages, &mut sink);
    sink.encoded
}

/// The AI SDK read-model strategy: a user/system message becomes `{ id, role,
/// parts }`; an assistant tool call becomes a `tool-<name>` part that its later
/// result mutates in place to `output-available` (there is no standalone tool
/// message in the AI SDK shape).
#[derive(Default)]
struct AiSdkHistorySink {
    encoded: Vec<Value>,
    /// tool_call_id -> (message index in `encoded`, part index in that message).
    pending_parts: std::collections::HashMap<String, (usize, usize)>,
}

impl HistorySink for AiSdkHistorySink {
    fn user_or_system(&mut self, id: &str, role: Role, content: &[ContentBlock]) {
        let role = if role == Role::User { "user" } else { "system" };
        self.encoded
            .push(history_message(id, role, text_parts(content)));
    }

    fn assistant(&mut self, id: &str, content: &[ContentBlock], tools: &[ToolUseRef<'_>]) {
        let mut parts = text_parts(content);
        let message_index = self.encoded.len();
        for tool in tools {
            let part_index = parts.len();
            parts.push(serde_json::json!({
                "type": format!("tool-{}", tool.name),
                "toolName": tool.name,
                "toolCallId": tool.id,
                "state": "input-available",
                "input": tool.input,
                "providerExecuted": true,
            }));
            self.pending_parts
                .insert(tool.id.to_string(), (message_index, part_index));
        }
        self.encoded.push(history_message(id, "assistant", parts));
    }

    fn tool_result(
        &mut self,
        _message_id: &str,
        _sub: usize,
        tool_use_id: &str,
        content: &[ContentBlock],
    ) {
        let Some((mi, pi)) = self.pending_parts.remove(tool_use_id) else {
            return;
        };
        if let Some(part) = self
            .encoded
            .get_mut(mi)
            .and_then(|m| m.get_mut("parts"))
            .and_then(Value::as_array_mut)
            .and_then(|parts| parts.get_mut(pi))
            .and_then(Value::as_object_mut)
        {
            part.insert("state".into(), Value::String("output-available".into()));
            part.insert("output".into(), parse_output(content));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id;
    use awaken_protocol_transport::{Pending, StepFailure, Terminal};
    use serde_json::json;

    // --- live `delta()` tier (the streamed prefix) ---

    fn td(t: &str) -> Delta {
        Delta::TextDelta { delta: t.into() }
    }
    fn tcd(id: &str, name: &str, args: &str) -> Delta {
        Delta::ToolCallDelta {
            id: id.into(),
            name: name.into(),
            args_delta: args.into(),
        }
    }

    #[test]
    fn delta_streams_text_then_tool_input_forwarding_suffixes() {
        let mut enc = AiSdkEncoder::new();
        let mut out = enc.fact(&Fact::RunStarted);
        out.extend(enc.delta(&td("Let me ")));
        out.extend(enc.delta(&td("read")));
        out.extend(enc.delta(&tcd("c1", "read", "")));
        out.extend(enc.delta(&tcd("c1", "read", "{\"path\":")));
        out.extend(enc.delta(&tcd("c1", "read", "\"x\"}")));
        out.extend(enc.finalize());
        assert_eq!(
            out,
            vec![
                UIStreamEvent::Start,
                UIStreamEvent::StartStep,
                UIStreamEvent::TextStart { id: "txt-0".into() },
                UIStreamEvent::TextDelta {
                    id: "txt-0".into(),
                    delta: "Let me ".into(),
                },
                UIStreamEvent::TextDelta {
                    id: "txt-0".into(),
                    delta: "read".into(),
                },
                // the tool call closes the open text run, then streams args suffixes
                UIStreamEvent::TextEnd { id: "txt-0".into() },
                UIStreamEvent::ToolInputStart {
                    tool_call_id: "c1".into(),
                    tool_name: "read".into(),
                },
                UIStreamEvent::ToolInputDelta {
                    tool_call_id: "c1".into(),
                    input_text_delta: "{\"path\":".into(),
                },
                UIStreamEvent::ToolInputDelta {
                    tool_call_id: "c1".into(),
                    input_text_delta: "\"x\"}".into(),
                },
            ]
        );
        assert!(enc.has_streamed());
    }

    #[test]
    fn delta_never_emits_authoritative_finish_or_input_available() {
        let mut enc = AiSdkEncoder::new();
        let mut out = enc.fact(&Fact::RunStarted);
        out.extend(enc.delta(&tcd("c1", "read", "{}")));
        out.extend(enc.finalize());
        assert!(out.iter().all(|e| !matches!(
            e,
            UIStreamEvent::Finish { .. }
                | UIStreamEvent::FinishStep
                | UIStreamEvent::ToolInputAvailable { .. }
        )));
        assert!(out.contains(&UIStreamEvent::ToolInputStart {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
        }));
    }

    #[test]
    fn a_repeated_run_started_is_idempotent() {
        let mut enc = AiSdkEncoder::new();
        let mut out = enc.fact(&Fact::RunStarted);
        out.extend(enc.fact(&Fact::RunStarted));
        assert_eq!(out, vec![UIStreamEvent::Start, UIStreamEvent::StartStep]);
    }

    #[test]
    fn delta_concurrent_tool_calls_stream_independently_by_call_id() {
        let mut enc = AiSdkEncoder::new();
        let mut out = Vec::new();
        for d in [
            tcd("c1", "read", ""),
            tcd("c2", "write", ""),
            tcd("c1", "read", "{\"a\":1}"),
            tcd("c2", "write", "{\"b\":2}"),
        ] {
            out.extend(enc.delta(&d));
        }
        assert!(out.contains(&UIStreamEvent::ToolInputStart {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
        }));
        assert!(out.contains(&UIStreamEvent::ToolInputStart {
            tool_call_id: "c2".into(),
            tool_name: "write".into(),
        }));
        assert!(out.contains(&UIStreamEvent::ToolInputDelta {
            tool_call_id: "c1".into(),
            input_text_delta: "{\"a\":1}".into(),
        }));
    }

    #[test]
    fn delta_reasoning_is_not_projected() {
        let mut enc = AiSdkEncoder::new();
        let out = enc.delta(&Delta::ReasoningDelta {
            delta: "hmm".into(),
        });
        assert!(out.is_empty());
    }

    /// A continuation-guard round (steering) is an audit lifecycle fact
    /// (`classify().live == false`) — it carries no AI-SDK wire part. Steering stays
    /// observable via the audit projection (`RunEvent::Continuation`), not this
    /// stream. This pins the omission so a future edit can't silently start
    /// leaking an internal guard round onto the wire.
    #[test]
    fn continuation_steering_is_not_projected() {
        let mut enc = AiSdkEncoder::new();
        for steered in [false, true] {
            let out = enc.fact(&Fact::Continuation {
                steered,
                detail: json!({ "reason": "auto_continue" }),
            });
            assert!(out.is_empty(), "steered={steered} projected {out:?}");
        }
    }

    #[test]
    fn terminal_failure_surfaces_an_error_frame() {
        let outcome = StepOutcome {
            terminal: Terminal::Failed(StepFailure {
                code: "inference_failed".into(),
                message: "upstream is down".into(),
            }),
            ..Default::default()
        };
        // A fresh (non-streamed) step: opened stream + error + finish("error").
        let step = encode_step(&outcome);
        assert!(
            step.iter()
                .any(|e| matches!(e, UIStreamEvent::Error { .. })),
            "error frame: {step:?}"
        );
        assert!(
            step.iter().any(|e| matches!(e, UIStreamEvent::Finish { finish_reason: Some(r), .. } if r == "error")),
            "finish(error): {step:?}",
        );
        // A streamed step's tail: error + finish("error") (prefix already emitted).
        let close = encode_close(&outcome);
        assert!(
            close
                .iter()
                .any(|e| matches!(e, UIStreamEvent::Error { .. })),
            "close error: {close:?}"
        );
    }

    /// Budget exhaustion (`Terminal::Exhausted` → `RunFinished{exhausted:true}`) is
    /// a clean terminus on the AI-SDK wire: the protocol has no "exhausted" finish
    /// reason, so it closes with `finish("stop")` like a natural end — never an
    /// error frame. Pins that exhaustion doesn't leak as a fault.
    #[test]
    fn an_exhausted_run_finishes_cleanly_not_as_error() {
        let outcome = StepOutcome {
            terminal: Terminal::Exhausted,
            ..Default::default()
        };
        let step = encode_step(&outcome);
        assert!(
            step.iter().any(
                |e| matches!(e, UIStreamEvent::Finish { finish_reason: Some(r), .. } if r == "stop")
            ),
            "finish(stop): {step:?}",
        );
        assert!(
            !step
                .iter()
                .any(|e| matches!(e, UIStreamEvent::Error { .. })),
            "an exhausted run is not an error: {step:?}",
        );
    }

    // REGRESSION (the live-merge, ADR-0058 Axis 9): a streamed step's committed
    // tail must NOT re-emit assistant text — the live `delta()` already streamed it
    // as `text-*`. `encode_close` drops the `AssistantMessage`, so the tail carries
    // only tool/finish frames. If that drop regressed, the client would see the
    // assistant text twice.
    #[test]
    fn streamed_text_is_not_re_emitted_by_the_committed_tail() {
        let outcome = StepOutcome {
            new_messages: vec![Message::text(
                Id("a1".into()),
                Role::Assistant,
                "hello there",
            )],
            terminal: Terminal::Finished,
        };
        let tail = encode_close(&outcome);
        assert!(
            tail.iter().all(|e| !matches!(
                e,
                UIStreamEvent::TextStart { .. }
                    | UIStreamEvent::TextDelta { .. }
                    | UIStreamEvent::TextEnd { .. }
            )),
            "the committed tail must not re-emit streamed text: {tail:?}"
        );
        // A non-streamed full projection DOES carry the text (nothing streamed it).
        let full = encode_step(&outcome);
        assert!(
            full.iter().any(
                |e| matches!(e, UIStreamEvent::TextDelta { delta, .. } if delta == "hello there")
            ),
            "the full projection carries the assistant text: {full:?}"
        );
    }

    fn assistant_tool(id: &str, call: &str, name: &str, args: Value) -> Message {
        Message {
            id: Id(id.to_string()),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: call.to_string(),
                name: name.to_string(),
                input: args,
            }],
        }
    }

    #[test]
    fn client_pending_tool_is_not_provider_executed() {
        let outcome = StepOutcome {
            new_messages: vec![assistant_tool("a1", "c1", "submit_answer", json!({}))],
            terminal: Terminal::Waiting {
                pending: Some(Pending {
                    tool_use_id: "c1".into(),
                    name: "submit_answer".into(),
                    input: json!({}),
                    client_executed: true,
                }),
            },
        };
        let events = encode_step(&outcome);
        let tool = events
            .iter()
            .find_map(|e| match e {
                UIStreamEvent::ToolInputAvailable {
                    provider_executed, ..
                } => Some(*provider_executed),
                _ => None,
            })
            .unwrap();
        assert!(!tool, "pending client tool must not be provider-executed");
        assert!(events.contains(&UIStreamEvent::finish("tool-calls")));
    }

    #[test]
    fn plain_assistant_turn_finishes_stop() {
        let outcome = StepOutcome {
            new_messages: vec![Message::text(Id("a1".into()), Role::Assistant, "hello")],
            terminal: Terminal::Finished,
        };
        let events = encode_step(&outcome);
        assert!(events.contains(&UIStreamEvent::finish("stop")));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, UIStreamEvent::TextDelta { delta, .. } if delta == "hello"))
        );
    }

    #[test]
    fn close_emits_authoritative_tail_without_start_or_text() {
        let outcome = StepOutcome {
            new_messages: vec![
                Message::text(Id("a1".into()), Role::Assistant, "let me read"),
                assistant_tool("a2", "c1", "read", json!({"path": "x"})),
            ],
            terminal: Terminal::Waiting {
                pending: Some(Pending {
                    tool_use_id: "c1".into(),
                    name: "read".into(),
                    input: json!({"path": "x"}),
                    client_executed: true,
                }),
            },
        };
        let events = encode_close(&outcome);
        // No start/step/text — those were streamed live.
        assert!(events.iter().all(|e| !matches!(
            e,
            UIStreamEvent::Start
                | UIStreamEvent::StartStep
                | UIStreamEvent::TextStart { .. }
                | UIStreamEvent::TextDelta { .. }
                | UIStreamEvent::TextEnd { .. }
        )));
        // The authoritative parsed input arrives, plus the finish frames.
        assert!(events.iter().any(|e| matches!(
            e,
            UIStreamEvent::ToolInputAvailable { tool_call_id, input, .. }
                if tool_call_id == "c1" && input == &json!({"path": "x"})
        )));
        assert!(events.contains(&UIStreamEvent::finish("tool-calls")));
    }

    #[test]
    fn history_merges_tool_call_with_result() {
        let messages = vec![
            Message::text(Id("u1".into()), Role::User, "go"),
            assistant_tool("a1", "c1", "read", json!({"path": "x"})),
            Message {
                id: Id("t1".into()),
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: vec![ContentBlock::Text {
                        text: "\"data\"".into(),
                    }],
                }],
            },
        ];
        let encoded = encode_history(&messages);
        assert_eq!(encoded.len(), 2);
        let part = &encoded[1]["parts"][0];
        assert_eq!(part["type"], "tool-read");
        assert_eq!(part["state"], "output-available");
        assert_eq!(part["output"], json!("data"));
    }

    #[test]
    fn a_tool_result_with_no_matching_call_is_dropped_from_history() {
        let messages = vec![
            Message::text(Id("u1".into()), Role::User, "go"),
            Message {
                id: Id("t1".into()),
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "orphan".into(),
                    content: vec![ContentBlock::Text {
                        text: "\"data\"".into(),
                    }],
                }],
            },
        ];
        let encoded = encode_history(&messages);
        // Only the user message survives; an orphan result has no call to merge into.
        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0]["role"], "user");
    }

    #[test]
    fn tool_result_error_maps_to_tool_output_error() {
        let events = AiSdkEncoder::new().fact(&Fact::ToolResult {
            id: "c1".into(),
            content: vec![ContentBlock::text("it broke")],
            is_error: true,
        });
        assert!(matches!(
            events.as_slice(),
            [UIStreamEvent::ToolOutputError { tool_call_id, error_text }]
                if tool_call_id == "c1" && error_text == "it broke"
        ));
    }

    #[test]
    fn tool_result_success_maps_to_tool_output_available() {
        let events = AiSdkEncoder::new().fact(&Fact::ToolResult {
            id: "c1".into(),
            content: vec![ContentBlock::text("ok")],
            is_error: false,
        });
        assert!(matches!(
            events.as_slice(),
            [UIStreamEvent::ToolOutputAvailable { tool_call_id, .. }] if tool_call_id == "c1"
        ));
    }

    #[test]
    fn run_failed_maps_to_error_then_finish_error() {
        let events = AiSdkEncoder::new().fact(&Fact::RunFailed {
            code: "overloaded".into(),
            message: "try later".into(),
        });
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            UIStreamEvent::Error { error_text }
                if error_text.contains("overloaded") && error_text.contains("try later")
        ));
        assert!(matches!(events[1], UIStreamEvent::FinishStep));
        assert!(matches!(&events[2], UIStreamEvent::Finish { .. }));
    }

    #[test]
    fn run_started_transcodes_to_start_and_start_step() {
        let events = AiSdkEncoder::new().fact(&Fact::RunStarted);
        assert!(matches!(
            events.as_slice(),
            [UIStreamEvent::Start, UIStreamEvent::StartStep]
        ));
    }

    #[test]
    fn waiting_transcodes_to_finish_step_then_tool_calls_finish() {
        let events = AiSdkEncoder::new().fact(&Fact::Waiting {
            pending_tool_use_id: Some("c1".into()),
        });
        assert!(matches!(events[0], UIStreamEvent::FinishStep));
        assert!(matches!(
            &events[1],
            UIStreamEvent::Finish { finish_reason: Some(r), .. } if r == "tool-calls"
        ));
    }

    #[test]
    fn a_tool_call_transcodes_to_tool_input_available() {
        use awaken_agent_contract::event::ToolDisposition;
        let events = AiSdkEncoder::new().fact(&Fact::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: json!({ "path": "x" }),
            disposition: ToolDisposition::PendingBuiltin,
        });
        assert!(matches!(
            &events[0],
            UIStreamEvent::ToolInputAvailable { tool_call_id, provider_executed: false, .. }
                if tool_call_id == "c1"
        ));
    }

    // A provider-executed (server-side) tool call must be flagged
    // `providerExecuted: true`, so `useChat` renders its result rather than asking
    // the client to run it — the complement of the pending-client/built-in rows.
    #[test]
    fn an_executed_tool_call_is_provider_executed() {
        let events = AiSdkEncoder::new().fact(&Fact::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: json!({ "path": "x" }),
            disposition: ToolDisposition::Executed,
        });
        assert!(matches!(
            events.as_slice(),
            [UIStreamEvent::ToolInputAvailable { tool_call_id, provider_executed: true, .. }]
                if tool_call_id == "c1"
        ));
    }

    // parse_output: a tool result whose text is not valid JSON falls back to a
    // JSON string (the other arm from the JSON-parsing `history_merges` row).
    #[test]
    fn a_non_json_tool_result_falls_back_to_a_string_output() {
        let events = AiSdkEncoder::new().fact(&Fact::ToolResult {
            id: "c1".into(),
            content: vec![ContentBlock::text("just text")],
            is_error: false,
        });
        assert!(matches!(
            events.as_slice(),
            [UIStreamEvent::ToolOutputAvailable { output, .. }] if *output == json!("just text")
        ));
    }

    // Post the projection change: an assistant message whose only block is an
    // empty-string Text is dropped by `fold_messages`, so the AI SDK stream
    // carries no spurious `text-*` frames — only the step's start and finish.
    #[test]
    fn an_empty_text_only_assistant_turn_emits_no_text_part() {
        let outcome = StepOutcome {
            new_messages: vec![Message::new(
                Id("a1".into()),
                Role::Assistant,
                vec![ContentBlock::text("")],
            )],
            terminal: Terminal::Finished,
        };
        let events = encode_step(&outcome);
        assert!(
            events.iter().all(|e| !matches!(
                e,
                UIStreamEvent::TextStart { .. }
                    | UIStreamEvent::TextDelta { .. }
                    | UIStreamEvent::TextEnd { .. }
            )),
            "an all-empty-text assistant turn must emit no text part: {events:?}"
        );
        assert!(events.contains(&UIStreamEvent::finish("stop")));
    }

    // The history fold drops the same all-empty-text assistant message, matching
    // the streaming projection — only the real user message survives.
    #[test]
    fn history_drops_an_empty_text_only_assistant_message() {
        let messages = vec![
            Message::text(Id("u1".into()), Role::User, "hi"),
            Message::new(
                Id("a1".into()),
                Role::Assistant,
                vec![ContentBlock::text("")],
            ),
        ];
        let encoded = encode_history(&messages);
        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0]["role"], "user");
    }
}
