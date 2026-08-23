//! Project a committed step (and thread history) into AI SDK UI Message Stream
//! parts. The step fold is shared (`awaken_agent_contract::event`); this module
//! owns the AI SDK *transcoder* — the `Fact -> UIStreamEvent` mapping — and
//! the history read-model fold.

use std::collections::BTreeSet;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::event::{
    Delta, Fact, HistorySink, StreamTerminalDecision, StreamTerminalKind, ToolDisposition,
    ToolUseRef, Transcoder, decide_stream_terminal, fold_history, fold_messages,
};
use awaken_session_contract::{StepOutcome, blocks_text};
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
    /// A terminal wire frame was emitted. Terminal state is absorbing.
    terminal_emitted: bool,
    /// Assistant text actually projected live. A dropped reasoning delta must
    /// not suppress the authoritative committed assistant message.
    streamed_text: bool,
    /// The id of the open live text block, if a text run is currently streaming.
    open_text: Option<String>,
    text_seq: usize,
    /// The private reasoning phase currently announced to the UI. Reasoning
    /// bytes are never projected; only its start/end lifecycle is visible.
    open_reasoning: Option<String>,
    reasoning_seq: usize,
    /// Call ids that already emitted `tool-input-start` live.
    tools: BTreeSet<String>,
}

impl AiSdkEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn close_text(&mut self) -> Vec<UIStreamEvent> {
        match self.open_text.take() {
            Some(id) => vec![UIStreamEvent::TextEnd { id }],
            None => Vec::new(),
        }
    }

    fn close_reasoning(&mut self) -> Vec<UIStreamEvent> {
        match self.open_reasoning.take() {
            Some(id) => vec![UIStreamEvent::ReasoningEnd { id }],
            None => Vec::new(),
        }
    }

    fn project_terminal(
        &mut self,
        requested: StreamTerminalKind,
        reconciliation_exact: bool,
        failure: Option<(String, String)>,
    ) -> Vec<UIStreamEvent> {
        let StreamTerminalDecision::Emit(kind) = decide_stream_terminal(
            self.started,
            self.terminal_emitted,
            reconciliation_exact,
            requested,
        ) else {
            return Vec::new();
        };
        self.terminal_emitted = true;
        match ai_sdk_terminal_class(kind) {
            AiSdkTerminalClass::ToolCalls => vec![
                UIStreamEvent::FinishStep,
                UIStreamEvent::finish("tool-calls"),
            ],
            AiSdkTerminalClass::Stop => {
                vec![UIStreamEvent::FinishStep, UIStreamEvent::finish("stop")]
            }
            AiSdkTerminalClass::Error => {
                let (code, message) = failure.unwrap_or_else(|| {
                    (
                        "stream_reconciliation_failed".to_string(),
                        "live stream does not match the committed outcome".to_string(),
                    )
                });
                vec![
                    UIStreamEvent::error(format!("{code}: {message}")),
                    UIStreamEvent::FinishStep,
                    UIStreamEvent::finish("error"),
                ]
            }
        }
    }

    /// Complete one stream through the same stateful encoder that projected its
    /// live prefix. Committed tool availability/output remains authoritative;
    /// assistant text is omitted only when text was actually projected live.
    pub fn complete(&mut self, outcome: &StepOutcome) -> Vec<UIStreamEvent> {
        if self.terminal_emitted {
            return Vec::new();
        }
        let pending = outcome
            .pending()
            .map(|pending| (pending.tool_use_id.as_str(), pending.client_executed));
        let events = fold_messages(&outcome.new_messages, pending);
        let committed_tools = events
            .iter()
            .filter_map(|event| match event {
                Fact::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let live_only = self
            .tools
            .difference(&committed_tools)
            .cloned()
            .collect::<Vec<_>>();

        let mut output = self.fact(&Fact::RunStarted);
        output.extend(self.close_reasoning());
        output.extend(self.close_text());
        for event in &events {
            if self.streamed_text && matches!(event, Fact::AssistantMessage { .. }) {
                continue;
            }
            output.extend(self.fact(event));
        }
        self.tools.clear();
        let terminal = outcome.terminal_event();
        let reconciliation_exact = live_only.is_empty();
        let failure = if reconciliation_exact {
            terminal_failure(&terminal)
        } else {
            Some((
                "stream_reconciliation_failed".to_string(),
                format!(
                    "live tool inputs missing from committed outcome: {}",
                    live_only.join(", ")
                ),
            ))
        };
        output.extend(self.project_terminal(
            terminal_kind(&terminal),
            reconciliation_exact,
            failure,
        ));
        output
    }

    pub fn fail(&mut self, message: impl Into<String>) -> Vec<UIStreamEvent> {
        if self.terminal_emitted {
            return Vec::new();
        }
        let mut output = self.fact(&Fact::RunStarted);
        output.extend(self.close_reasoning());
        output.extend(self.close_text());
        self.tools.clear();
        output.extend(self.project_terminal(
            StreamTerminalKind::Failed,
            true,
            Some(("stream_failed".to_string(), message.into())),
        ));
        output
    }
}

impl Transcoder for AiSdkEncoder {
    type Output = UIStreamEvent;

    fn fact(&mut self, event: &Fact) -> Vec<UIStreamEvent> {
        if self.terminal_emitted
            && !matches!(
                event,
                Fact::Awaiting { .. } | Fact::RunFinished { .. } | Fact::RunFailed { .. }
            )
        {
            return Vec::new();
        }
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
            } => {
                let mut events = vec![UIStreamEvent::ToolInputAvailable {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    input: input.clone(),
                    // Built-ins still execute on the server after approval;
                    // only PendingClient asks the browser to execute a tool.
                    provider_executed: !matches!(disposition, ToolDisposition::PendingClient),
                }];
                if matches!(disposition, ToolDisposition::PendingBuiltin) {
                    events.push(UIStreamEvent::ToolApprovalRequest {
                        approval_id: id.clone(),
                        tool_call_id: id.clone(),
                    });
                }
                events
            }
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
            Fact::Awaiting { .. } => {
                self.project_terminal(StreamTerminalKind::Awaiting, true, None)
            }
            Fact::RunFinished { .. } => {
                self.project_terminal(StreamTerminalKind::Finished, true, None)
            }
            Fact::RunFailed { code, message } => self.project_terminal(
                StreamTerminalKind::Failed,
                true,
                Some((code.clone(), message.clone())),
            ),
            // An internal continuation-guard round is not an AI-SDK wire part; the
            // committed fold never emits it into this stream.
            // The committed thinking fact has no content and the live delta path
            // already owns the standard start/end lifecycle, so do not duplicate it.
            Fact::Continuation { .. } | Fact::AssistantThinking => Vec::new(),
        }
    }

    fn delta(&mut self, delta: &Delta) -> Vec<UIStreamEvent> {
        if self.terminal_emitted {
            return Vec::new();
        }
        match delta {
            Delta::TextDelta { delta } => {
                self.streamed_text = true;
                let mut out = self.close_reasoning();
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
                let mut out = self.close_reasoning();
                out.extend(self.close_text());
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
            // Keep private reasoning private while still making its lifecycle
            // visible through AI SDK's standard reasoning part.
            Delta::ReasoningDelta { .. } => {
                let mut out = self.close_text();
                if self.open_reasoning.is_none() {
                    let id = format!("reasoning-{}", self.reasoning_seq);
                    self.reasoning_seq += 1;
                    self.open_reasoning = Some(id.clone());
                    out.push(UIStreamEvent::ReasoningStart { id });
                }
                out
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AiSdkTerminalClass {
    ToolCalls,
    Stop,
    Error,
}

const fn ai_sdk_terminal_class(kind: StreamTerminalKind) -> AiSdkTerminalClass {
    match kind {
        StreamTerminalKind::Awaiting => AiSdkTerminalClass::ToolCalls,
        StreamTerminalKind::Finished => AiSdkTerminalClass::Stop,
        StreamTerminalKind::Failed => AiSdkTerminalClass::Error,
    }
}

fn terminal_kind(fact: &Fact) -> StreamTerminalKind {
    match fact {
        Fact::Awaiting { .. } => StreamTerminalKind::Awaiting,
        Fact::RunFinished { .. } => StreamTerminalKind::Finished,
        Fact::RunFailed { .. } => StreamTerminalKind::Failed,
        _ => unreachable!("only terminal facts reach terminal projection"),
    }
}

fn terminal_failure(fact: &Fact) -> Option<(String, String)> {
    match fact {
        Fact::RunFailed { code, message } => Some((code.clone(), message.clone())),
        _ => None,
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn ai_sdk_terminal_category_mapping_is_total_and_exact() {
        let tag: u8 = kani::any();
        kani::assume(tag <= 2);
        let kind = match tag {
            0 => StreamTerminalKind::Awaiting,
            1 => StreamTerminalKind::Finished,
            _ => StreamTerminalKind::Failed,
        };
        let projected = ai_sdk_terminal_class(kind);
        match projected {
            AiSdkTerminalClass::ToolCalls => assert_eq!(kind, StreamTerminalKind::Awaiting),
            AiSdkTerminalClass::Stop => assert_eq!(kind, StreamTerminalKind::Finished),
            AiSdkTerminalClass::Error => assert_eq!(kind, StreamTerminalKind::Failed),
        }
    }
}

/// Project one committed step into an ordered UI Message Stream. Each response is a
/// self-contained stream: `start` … `finish`.
pub fn encode_step(outcome: &StepOutcome) -> Vec<UIStreamEvent> {
    AiSdkEncoder::new().complete(outcome)
}

/// Parse a tool result's text as JSON, falling back to a string.
fn parse_output(content: &[ContentBlock]) -> Value {
    let text = blocks_text(content);
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

/// Fold committed thread messages into AI SDK `UIMessage`s for the history
/// endpoint. Assistant tool calls merge with their later tool result into a single
/// `output-available` part (`providerExecuted: true`). The current runtime
/// `Pending` fact is supplied so reloads preserve the same client-tool versus
/// server-approval distinction as the live projection. This is a read-model
/// fold, distinct from the streaming projection above; the shared walk lives in
/// [`fold_history`], this sink only shapes each message the AI SDK way.
pub fn encode_history(
    messages: &[Message],
    pending: Option<&awaken_session_contract::Pending>,
) -> Vec<Value> {
    let mut sink = AiSdkHistorySink::new(pending);
    fold_history(messages, &mut sink);
    sink.encoded
}

/// The AI SDK read-model strategy: a user/system message becomes `{ id, role,
/// parts }`; an assistant tool call becomes a `tool-<name>` part that its later
/// result mutates in place to `output-available` (there is no standalone tool
/// message in the AI SDK shape).
struct AiSdkHistorySink {
    encoded: Vec<Value>,
    /// tool_call_id -> (message index in `encoded`, part index in that message).
    pending_parts: std::collections::HashMap<String, (usize, usize)>,
    /// The sole current wait from the runtime; older unresolved-looking calls
    /// are historical provider calls, not additional decisions.
    current_wait: Option<(String, bool)>,
}

impl AiSdkHistorySink {
    fn new(pending: Option<&awaken_session_contract::Pending>) -> Self {
        Self {
            encoded: Vec::new(),
            pending_parts: std::collections::HashMap::new(),
            current_wait: pending.map(|value| (value.tool_use_id.clone(), value.client_executed)),
        }
    }
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
            let current_wait = self.current_wait.as_ref().filter(|(id, _)| id == tool.id);
            let awaiting_builtin = matches!(current_wait, Some((_, false)));
            let mut part = serde_json::json!({
                "type": format!("tool-{}", tool.name),
                "toolName": tool.name,
                "toolCallId": tool.id,
                "state": if awaiting_builtin { "approval-requested" } else { "input-available" },
                "input": tool.input,
                "providerExecuted": !matches!(current_wait, Some((_, true))),
            });
            if awaiting_builtin {
                part["approval"] = serde_json::json!({ "id": tool.id });
            }
            parts.push(part);
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
    use awaken_agent_contract::agent::run::{EndCause, Failure};
    use awaken_session_contract::Pending;
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
        assert!(enc.streamed_text);
    }

    #[test]
    fn delta_never_emits_authoritative_finish_or_input_available() {
        let mut enc = AiSdkEncoder::new();
        let mut out = enc.fact(&Fact::RunStarted);
        out.extend(enc.delta(&tcd("c1", "read", "{}")));
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
    fn delta_reasoning_projects_only_a_bounded_private_lifecycle() {
        /* Reasoning visibility decision table. Causes: C1 one or more private
         * reasoning deltas arrive; C2 public text or a tool delta follows; C3
         * the run terminates while reasoning is open. Effects: E1 emit one
         * standard reasoning-start without content; E2 emit one reasoning-end
         * before the next public part; E3 never emit reasoning-delta bytes.
         * Rules: R1 C1=>E1+E3; R2 C1+C2=>E2; R3 C1+C3=>E2. */
        let mut enc = AiSdkEncoder::new();
        let mut out = enc.delta(&Delta::ReasoningDelta {
            delta: "hmm".into(),
        });
        out.extend(enc.delta(&Delta::ReasoningDelta {
            delta: " still private".into(),
        }));
        out.extend(enc.delta(&td("answer")));
        assert_eq!(
            out,
            vec![
                UIStreamEvent::ReasoningStart {
                    id: "reasoning-0".into()
                },
                UIStreamEvent::ReasoningEnd {
                    id: "reasoning-0".into()
                },
                UIStreamEvent::TextStart { id: "txt-0".into() },
                UIStreamEvent::TextDelta {
                    id: "txt-0".into(),
                    delta: "answer".into()
                },
            ]
        );
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
        // Causes: the fixtures below establish `terminal failure` with the concrete inputs, state,
        // dependencies, and failure triggers used by this case.
        // Effects: the observable result `surfaces an error frame` and every asserted state
        // transition or side effect must hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect rule F1: a committed inference failure emits an error
        // frame and finish(error), never an ordinary finish.
        let outcome = StepOutcome::ended(
            Vec::new(),
            EndCause::Error(Failure::Inference {
                code: "inference_failed".into(),
                message: "upstream is down".into(),
            }),
        );
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
        let mut encoder = AiSdkEncoder::new();
        encoder.fact(&Fact::RunStarted);
        let close = encoder.complete(&outcome);
        assert!(
            close
                .iter()
                .any(|e| matches!(e, UIStreamEvent::Error { .. })),
            "close error: {close:?}"
        );
    }

    /// Budget exhaustion (`EndCause::MaxSteps` → `RunFinished{exhausted:true}`) is
    /// a clean end on the AI-SDK wire: the protocol has no "exhausted" finish
    /// reason, so it closes with `finish("stop")` like a natural end — never an
    /// error frame. Pins that exhaustion doesn't leak as a fault.
    #[test]
    fn an_exhausted_run_finishes_cleanly_not_as_error() {
        // Causes: the fixtures below establish `an exhausted run finishes cleanly not as error`
        // with the concrete inputs, state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `all output, state, side-effect, error, and terminal
        // assertions below hold together` and every asserted state transition or side effect must
        // hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        let outcome = StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::MaxSteps,
        );
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
    // as `text-*`. Stateful completion drops the `AssistantMessage`, so the tail carries
    // only tool/finish frames. If that drop regressed, the client would see the
    // assistant text twice.
    #[test]
    fn streamed_text_is_not_re_emitted_by_the_committed_tail() {
        // Causes: the fixtures below establish `streamed text` with the concrete inputs, state,
        // dependencies, and failure triggers used by this case.
        // Effects: the observable result `is not re emitted by the committed tail` and every
        // asserted state transition or side effect must hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Coverage rationale: `streamed text` is one independent branch selecting `is not re
        // emitted by the committed tail`; a multi-row decision table is not applicable, and sibling
        // tests own alternate causes.
        let outcome = StepOutcome::ended(
            vec![Message::text(
                Id("a1".into()),
                Role::Assistant,
                "hello there",
            )],
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        );
        let mut encoder = AiSdkEncoder::new();
        encoder.fact(&Fact::RunStarted);
        encoder.delta(&Delta::TextDelta {
            delta: "hello there".into(),
        });
        let tail = encoder.complete(&outcome);
        assert!(
            tail.iter().all(|e| !matches!(
                e,
                UIStreamEvent::TextStart { .. } | UIStreamEvent::TextDelta { .. }
            )),
            "the committed tail must not re-emit streamed text: {tail:?}"
        );
        assert_eq!(
            tail.iter()
                .filter(|event| matches!(event, UIStreamEvent::TextEnd { .. }))
                .count(),
            1,
            "completion closes the live text run exactly once: {tail:?}"
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
        // Causes: the fixtures below establish `client pending tool` with the concrete inputs,
        // state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `is not provider executed` and every asserted state
        // transition or side effect must hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Coverage rationale: `client pending tool` is one independent branch selecting `is not
        // provider executed`; a multi-row decision table is not applicable, and sibling tests own
        // alternate causes.
        let outcome = StepOutcome::awaiting(
            vec![assistant_tool("a1", "c1", "submit_answer", json!({}))],
            Some(Pending {
                tool_use_id: "c1".into(),
                name: "submit_answer".into(),
                input: json!({}),
                client_executed: true,
            }),
        );
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
        // Causes: the fixtures below establish `plain assistant turn finishes stop` with the
        // concrete inputs, state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `all output, state, side-effect, error, and terminal
        // assertions below hold together` and every asserted state transition or side effect must
        // hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Coverage rationale: `plain assistant turn finishes stop` is one independent branch
        // selecting `all output, state, side-effect, error, and terminal assertions below hold
        // together`; a multi-row decision table is not applicable, and sibling tests own alternate
        // causes.
        let outcome = StepOutcome::ended(
            vec![Message::text(Id("a1".into()), Role::Assistant, "hello")],
            EndCause::NaturalEnd,
        );
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
        // Causes: the fixtures below establish `close` with the concrete inputs, state,
        // dependencies, and failure triggers used by this case.
        // Effects: the observable result `emits authoritative tail without start or text` and every
        // asserted state transition or side effect must hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Coverage rationale: `close` is one independent branch selecting `emits authoritative tail
        // without start or text`; a multi-row decision table is not applicable, and sibling tests
        // own alternate causes.
        let outcome = StepOutcome::awaiting(
            vec![
                Message::text(Id("a1".into()), Role::Assistant, "let me read"),
                assistant_tool("a2", "c1", "read", json!({"path": "x"})),
            ],
            Some(Pending {
                tool_use_id: "c1".into(),
                name: "read".into(),
                input: json!({"path": "x"}),
                client_executed: true,
            }),
        );
        let mut encoder = AiSdkEncoder::new();
        encoder.fact(&Fact::RunStarted);
        encoder.delta(&Delta::TextDelta {
            delta: "let me read".into(),
        });
        encoder.delta(&Delta::ToolCallDelta {
            id: "c1".into(),
            name: "read".into(),
            args_delta: "{\"path\":\"x\"}".into(),
        });
        let events = encoder.complete(&outcome);
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
    fn reasoning_only_prefix_keeps_committed_text_without_restarting_stream() {
        // Causes: the fixtures below establish `reasoning only prefix` with the concrete inputs,
        // state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `keeps committed text without restarting stream` and every
        // asserted state transition or side effect must hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Coverage rationale: `reasoning only prefix` is one independent branch selecting `keeps
        // committed text without restarting stream`; a multi-row decision table is not applicable,
        // and sibling tests own alternate causes.
        // CE-AI3/AI4: private reasoning bytes are not visible text. Completion
        // closes the activity marker, emits the committed answer, and does not
        // repeat start frames.
        let outcome = StepOutcome::ended(
            vec![Message::text(Id("a1".into()), Role::Assistant, "answer")],
            EndCause::NaturalEnd,
        );
        let mut encoder = AiSdkEncoder::new();
        assert_eq!(encoder.fact(&Fact::RunStarted).len(), 2);
        assert_eq!(
            encoder.delta(&Delta::ReasoningDelta {
                delta: "hmm".into()
            }),
            vec![UIStreamEvent::ReasoningStart {
                id: "reasoning-0".into()
            }],
        );
        let events = encoder.complete(&outcome);
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, UIStreamEvent::Start | UIStreamEvent::StartStep))
        );
        assert!(events.iter().any(
            |event| matches!(event, UIStreamEvent::TextDelta { delta, .. } if delta == "answer")
        ));
        assert!(events.contains(&UIStreamEvent::ReasoningEnd {
            id: "reasoning-0".into()
        }));
    }

    #[test]
    fn live_tool_missing_from_committed_outcome_fails_closed() {
        // Causes: the fixtures below establish `live tool missing from committed outcome fails
        // closed` with the concrete inputs, state, dependencies, and failure triggers used by this
        // case.
        // Effects: the observable result `all output, state, side-effect, error, and terminal
        // assertions below hold together` and every asserted state transition or side effect must
        // hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Coverage rationale: `live tool missing from committed outcome fails closed` is one
        // independent branch selecting `all output, state, side-effect, error, and terminal
        // assertions below hold together`; a multi-row decision table is not applicable, and
        // sibling tests own alternate causes.
        // CE-AI9: tool-input-start without a committed ToolCall cannot receive an
        // authoritative input-available frame, so completion terminates as error.
        let mut encoder = AiSdkEncoder::new();
        encoder.delta(&Delta::ToolCallDelta {
            id: "c1".into(),
            name: "read".into(),
            args_delta: "{".into(),
        });
        let events = encoder.complete(&StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, UIStreamEvent::Error { .. }))
        );
        assert!(events.iter().any(
            |event| matches!(event, UIStreamEvent::Finish { finish_reason: Some(reason), .. } if reason == "error")
        ));
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
                    is_error: false,
                }],
            },
        ];
        let encoded = encode_history(&messages, None);
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
                    is_error: false,
                }],
            },
        ];
        let encoded = encode_history(&messages, None);
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
        let mut encoder = AiSdkEncoder::new();
        encoder.fact(&Fact::RunStarted);
        let events = encoder.fact(&Fact::RunFailed {
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
    fn awaiting_transcodes_to_finish_step_then_tool_calls_finish() {
        let mut encoder = AiSdkEncoder::new();
        encoder.fact(&Fact::RunStarted);
        let events = encoder.fact(&Fact::Awaiting {
            pending_tool_use_id: Some("c1".into()),
        });
        assert!(matches!(events[0], UIStreamEvent::FinishStep));
        assert!(matches!(
            &events[1],
            UIStreamEvent::Finish { finish_reason: Some(r), .. } if r == "tool-calls"
        ));
    }

    #[test]
    fn terminal_requires_start_and_is_absorbing() {
        let terminal = Fact::RunFinished { exhausted: false };
        let mut encoder = AiSdkEncoder::new();
        assert!(
            encoder.fact(&terminal).is_empty(),
            "an unstarted stream cannot finish"
        );
        assert_eq!(
            encoder.fact(&Fact::RunStarted),
            vec![UIStreamEvent::Start, UIStreamEvent::StartStep]
        );
        assert!(matches!(
            encoder.fact(&terminal).as_slice(),
            [UIStreamEvent::FinishStep, UIStreamEvent::Finish { .. }]
        ));
        assert!(
            encoder.fact(&terminal).is_empty(),
            "terminal must not repeat"
        );
        assert!(
            encoder
                .delta(&Delta::TextDelta {
                    delta: "late".into()
                })
                .is_empty(),
            "terminal state must not be strengthened by later content"
        );
    }

    #[test]
    fn pending_builtin_transcodes_to_server_tool_plus_approval_request() {
        /* Tool-disposition decision table. C1 built-in tool awaits permission;
         * C2 client tool awaits browser output; C3 server tool already ran.
         * E1 server-executed input + approval request; E2 client-executed
         * input only; E3 server-executed historical input only.
         * R1=C1=>E1; R2=C2=>E2; R3=C3=>E3. */
        use awaken_agent_contract::event::ToolDisposition;
        let events = AiSdkEncoder::new().fact(&Fact::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: json!({ "path": "x" }),
            disposition: ToolDisposition::PendingBuiltin,
        });
        assert!(matches!(
            events.as_slice(),
            [
                UIStreamEvent::ToolInputAvailable { tool_call_id, provider_executed: true, .. },
                UIStreamEvent::ToolApprovalRequest { approval_id, tool_call_id: approval_call_id },
            ] if tool_call_id == "c1" && approval_id == "c1" && approval_call_id == "c1"
        ));
        assert_eq!(
            serde_json::to_value(&events[1]).expect("approval event serializes"),
            json!({
                "type": "tool-approval-request",
                "approvalId": "c1",
                "toolCallId": "c1"
            })
        );

        let client = AiSdkEncoder::new().fact(&Fact::ToolCall {
            id: "c2".into(),
            name: "browser_probe".into(),
            input: json!({}),
            disposition: ToolDisposition::PendingClient,
        });
        assert!(matches!(
            client.as_slice(),
            [UIStreamEvent::ToolInputAvailable {
                provider_executed: false,
                ..
            }]
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
        // Causes: the fixtures below establish `an empty text only assistant turn` with the
        // concrete inputs, state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `emits no text part` and every asserted state transition
        // or side effect must hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Coverage rationale: `an empty text only assistant turn` is one independent branch
        // selecting `emits no text part`; a multi-row decision table is not applicable, and sibling
        // tests own alternate causes.
        let outcome = StepOutcome::ended(
            vec![Message::new(
                Id("a1".into()),
                Role::Assistant,
                vec![ContentBlock::text("")],
            )],
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        );
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
        let encoded = encode_history(&messages, None);
        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0]["role"], "user");
    }

    #[test]
    fn history_preserves_the_runtime_owned_current_tool_wait() {
        /* Reload projection decision table. C1 the unmatched call is the
         * current built-in wait; C2 it is the current client wait; C3 no wait
         * identifies it. E1 approval-requested/providerExecuted; E2
         * input-available/client-executed; E3 historical provider-executed.
         * R1=C1=>E1; R2=C2=>E2; R3=C3=>E3. */
        let messages = vec![assistant_tool("a1", "c1", "bash", json!({"command":"pwd"}))];
        let builtin = Pending {
            tool_use_id: "c1".into(),
            name: "bash".into(),
            input: json!({"command":"pwd"}),
            client_executed: false,
        };
        let client = Pending {
            client_executed: true,
            ..builtin.clone()
        };

        let builtin_part = encode_history(&messages, Some(&builtin))[0]["parts"][0].clone();
        assert_eq!(builtin_part["state"], "approval-requested");
        assert_eq!(builtin_part["providerExecuted"], true);
        assert_eq!(builtin_part["approval"]["id"], "c1");

        let client_part = encode_history(&messages, Some(&client))[0]["parts"][0].clone();
        assert_eq!(client_part["state"], "input-available");
        assert_eq!(client_part["providerExecuted"], false);

        let historical_part = encode_history(&messages, None)[0]["parts"][0].clone();
        assert_eq!(historical_part["state"], "input-available");
        assert_eq!(historical_part["providerExecuted"], true);
    }
}
