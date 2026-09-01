//! AI SDK request decoding: thread id, new-message conversion, and tool-call
//! decision extraction. All AI SDK–specific parsing lives here so the router only
//! routes.

use std::collections::HashMap;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::fresh_process_id;
use serde_json::Value;

use crate::types::{AiSdkChatRequest, AwakenFileKind, UIMessage, UIMessagePart};

/// A client's answer to an awaiting tool, before binding-specific translation.
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionKind {
    /// `output-available`: the tool produced a result.
    Output(Value),
    /// `output-error`: the tool failed.
    Error(String),
    /// `output-denied` or `approval-responded { approved: false }`.
    Denied,
    /// `approval-responded { approved: true }`.
    Approved,
}

/// One extracted tool-call decision keyed by its call id.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub tool_call_id: String,
    pub kind: DecisionKind,
}

/// A decoded request ready for the runtime.
pub struct ProcessedRequest {
    pub operation_id: String,
    pub thread_id: String,
    pub messages: Vec<Message>,
    pub decisions: Vec<Decision>,
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessError {
    #[error("AI SDK committed history contains duplicate message id `{0}`")]
    DuplicateHistoryId(String),
    #[error("AI SDK message id `{0}` conflicts with its committed role")]
    CommittedRoleConflict(String),
    #[error("AI SDK turn requires exactly one current user or system input message")]
    InputCardinality,
    #[error("AI SDK request contains neither a turn nor a tool decision")]
    EmptyOperation,
}

impl ProcessedRequest {
    /// True when the request carries only decisions (no new content) — a resume.
    pub fn is_resume_only(&self) -> bool {
        self.messages.is_empty() && !self.decisions.is_empty()
    }
}

/// Decode one AI SDK `sendMessage` or tool-decision request. Committed history
/// is used only to reject role spoofing and distinguish older transcript entries
/// from the single current input; durable Run admission remains the replay and
/// payload-conflict authority.
pub fn process_request(
    payload: AiSdkChatRequest,
    committed_history: &[Message],
) -> Result<ProcessedRequest, ProcessError> {
    let thread_id = payload
        .thread_id
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| fresh_process_id("thread"));

    let known_roles = committed_roles(committed_history)?;
    validate_committed_roles(&payload.messages, &known_roles)?;
    let current_input = current_input_message(&payload.messages, &known_roles)?;
    let (operation_id, messages, decisions) = match current_input {
        Some(current) => {
            let operation_id = normalized_id(current.id.as_deref())
                .map(str::to_string)
                .unwrap_or_else(|| fresh_process_id("ai-sdk-operation"));
            let message = convert_input_message(current, &operation_id)
                .expect("current input was classified from the same role/content converter");
            (operation_id, vec![message], Vec::new())
        }
        None => {
            let extracted = extract_decisions(payload.messages.last());
            if extracted.decisions.is_empty() {
                return Err(ProcessError::EmptyOperation);
            }
            (
                extracted
                    .operation_id
                    .unwrap_or_else(|| fresh_process_id("ai-sdk-operation")),
                Vec::new(),
                extracted.decisions,
            )
        }
    };

    Ok(ProcessedRequest {
        operation_id,
        thread_id,
        messages,
        decisions,
        agent_id: payload.agent_id,
    })
}

fn committed_roles(history: &[Message]) -> Result<HashMap<&str, Role>, ProcessError> {
    let mut roles = HashMap::with_capacity(history.len());
    for message in history {
        if roles.insert(message.id.0.as_str(), message.role).is_some() {
            return Err(ProcessError::DuplicateHistoryId(message.id.0.clone()));
        }
    }
    Ok(roles)
}

fn normalized_id(id: Option<&str>) -> Option<&str> {
    id.map(str::trim).filter(|id| !id.is_empty())
}

fn wire_role(role: &str) -> Option<Role> {
    match role {
        "system" => Some(Role::System),
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "tool" => Some(Role::Tool),
        _ => None,
    }
}

fn validate_committed_roles(
    messages: &[UIMessage],
    known_roles: &HashMap<&str, Role>,
) -> Result<(), ProcessError> {
    for message in messages {
        let Some(id) = normalized_id(message.id.as_deref()) else {
            continue;
        };
        let Some(committed) = known_roles.get(id) else {
            continue;
        };
        if wire_role(&message.role) != Some(*committed) {
            return Err(ProcessError::CommittedRoleConflict(id.to_string()));
        }
    }
    Ok(())
}

/// Select the one current `sendMessage` input. Older input-shaped entries must
/// already exist in committed history; two unknown inputs are an unsupported
/// batch and fail before Session Run admission. A known final input is retained
/// for response-loss replay and downstream exact/conflict classification.
fn current_input_message<'a>(
    messages: &'a [UIMessage],
    known_roles: &HashMap<&str, Role>,
) -> Result<Option<&'a UIMessage>, ProcessError> {
    let trailing_inputs = messages
        .iter()
        .rev()
        .take_while(|message| convert_input_message(message, "probe").is_some())
        .count();
    if trailing_inputs > 1 {
        return Err(ProcessError::InputCardinality);
    }
    let current = messages
        .last()
        .filter(|message| convert_input_message(message, "probe").is_some());
    for message in messages.iter().take(messages.len().saturating_sub(1)) {
        if convert_input_message(message, "probe").is_none() {
            continue;
        }
        let is_committed_history =
            normalized_id(message.id.as_deref()).is_some_and(|id| known_roles.contains_key(id));
        if !is_committed_history {
            return Err(ProcessError::InputCardinality);
        }
    }
    Ok(current)
}

fn convert_input_message(message: &UIMessage, fallback_id: &str) -> Option<Message> {
    let role = match message.role.as_str() {
        "user" => Role::User,
        "system" => Role::System,
        _ => return None,
    };
    let blocks = blocks_of(&message.parts);
    if blocks.is_empty() {
        return None;
    }
    let id = normalized_id(message.id.as_deref()).unwrap_or(fallback_id);
    Some(Message::new(MessageId(id.to_string()), role, blocks))
}

/// Convert a UI message's parts into neutral content blocks: `text` parts become
/// text; `file` parts with an image media type become an image block (inline
/// `data:` base64 preferred, else a remote URL). Non-content parts (`tool-*`,
/// `step-start`, reasoning) are dropped — they are not user-visible input.
fn blocks_of(parts: &[UIMessagePart]) -> Vec<ContentBlock> {
    parts.iter().filter_map(part_to_block).collect()
}

fn part_to_block(part: &UIMessagePart) -> Option<ContentBlock> {
    match part {
        UIMessagePart::Text { text } => (!text.is_empty()).then(|| ContentBlock::text(text)),
        UIMessagePart::File { media_type, url } => {
            if !media_type.starts_with("image/") {
                return None;
            }
            Some(match parse_data_uri(url) {
                Some((mime, data)) => ContentBlock::image_base64(mime, data),
                None => ContentBlock::image_url(url.clone()),
            })
        }
        UIMessagePart::AwakenFile { file_id, kind } => Some(match kind {
            AwakenFileKind::Image => ContentBlock::image_file(file_id),
            AwakenFileKind::Document => ContentBlock::document_file(file_id),
        }),
        UIMessagePart::Tool(_) | UIMessagePart::Other { .. } => None,
    }
}

/// Split a `data:<mime>;base64,<data>` URI into its mime type and base64 payload.
fn parse_data_uri(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    Some((mime.to_string(), data.to_string()))
}

struct ExtractedDecisions {
    operation_id: Option<String>,
    decisions: Vec<Decision>,
}

/// Extract tool-call decisions from assistant `tool-*` parts. A part run by the
/// provider (`providerExecuted: true`) is historical, not a fresh decision. The
/// operation id belongs to the final message that actually contributed a fresh
/// decision; an id-less decision never inherits an earlier history id.
fn extract_decisions(message: Option<&UIMessage>) -> ExtractedDecisions {
    let mut decisions = Vec::new();
    let Some(message) = message.filter(|message| message.role == "assistant") else {
        return ExtractedDecisions {
            operation_id: None,
            decisions,
        };
    };
    for part in &message.parts {
        let UIMessagePart::Tool(tool) = part else {
            continue;
        };
        let (Some(state), Some(tool_call_id)) = (tool.state.as_deref(), tool.tool_call_id.clone())
        else {
            continue;
        };
        // Provider-executed outputs are history. A provider-executed
        // `approval-responded` part is different: the server owns execution,
        // while the user owns the permission decision that resumes it.
        if tool.provider_executed && state != "approval-responded" {
            continue;
        }
        let kind = match state {
            "output-available" => DecisionKind::Output(tool.output.clone().unwrap_or(Value::Null)),
            "output-error" => DecisionKind::Error(
                tool.error_text
                    .clone()
                    .unwrap_or_else(|| "tool execution error".to_string()),
            ),
            "output-denied" => DecisionKind::Denied,
            "approval-responded" => {
                if tool.approval.as_ref().map(|a| a.approved).unwrap_or(false) {
                    DecisionKind::Approved
                } else {
                    DecisionKind::Denied
                }
            }
            _ => continue,
        };
        decisions.push(Decision { tool_call_id, kind });
    }
    let operation_id = if decisions.is_empty() {
        None
    } else {
        normalized_id(message.id.as_deref()).map(str::to_string)
    };
    ExtractedDecisions {
        operation_id,
        decisions,
    }
}

/// Render a decision result value as tool-result text.
pub fn result_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_contract::blocks_text;
    use serde_json::json;

    fn ui(role: &str, id: &str, parts: Vec<Value>) -> UIMessage {
        UIMessage {
            id: Some(id.to_string()),
            role: role.to_string(),
            parts: parts
                .into_iter()
                .map(|part| serde_json::from_value(part).expect("valid UI part fixture"))
                .collect(),
        }
    }

    fn process(req: AiSdkChatRequest, history: &[Message]) -> ProcessedRequest {
        process_request(req, history).expect("valid AI SDK request fixture")
    }

    fn rejection(req: AiSdkChatRequest, history: &[Message]) -> ProcessError {
        match process_request(req, history) {
            Ok(_) => panic!("AI SDK request fixture must be rejected"),
            Err(error) => error,
        }
    }

    fn committed(id: &str, role: Role, text: &str) -> Message {
        Message::text(MessageId(id.to_string()), role, text)
    }

    #[test]
    fn converts_the_current_user_text_and_skips_assistant_history() {
        let req = AiSdkChatRequest {
            messages: vec![
                ui(
                    "assistant",
                    "a1",
                    vec![json!({"type":"text","text":"prior"})],
                ),
                ui("user", "u1", vec![json!({"type":"text","text":"hi"})]),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process(req, &[]);
        assert_eq!(p.thread_id, "t");
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].role, Role::User);
    }

    #[test]
    fn decodes_image_file_part_into_an_image_block() {
        let req = AiSdkChatRequest {
            messages: vec![ui(
                "user",
                "u1",
                vec![
                    json!({"type":"file","mediaType":"image/png","url":"data:image/png;base64,AAAA"}),
                    json!({"type":"text","text":"what is this"}),
                ],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process(req, &[]);
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].content.len(), 2);
        assert!(matches!(
            p.messages[0].content[0],
            ContentBlock::Image {
                source: awaken_agent_contract::agent::content::ImageSource::Base64 { .. }
            }
        ));
        assert_eq!(blocks_text(&p.messages[0].content), "what is this");
    }

    #[test]
    fn decodes_preuploaded_file_data_parts_without_url_or_bytes() {
        // Decision table: image/document kind selects the matching neutral File
        // source; both retain only the opaque id for attempt-bound authorization.
        use awaken_agent_contract::agent::content::{DocumentSource, ImageSource};
        let req = AiSdkChatRequest {
            messages: vec![ui(
                "user",
                "u1",
                vec![
                    json!({
                        "type":"data-awaken-file",
                        "data": {
                            "object":"awaken.file_reference",
                            "fileRef":"file_0123456789abcdef0123456789abcdef",
                            "kind":"image"
                        }
                    }),
                    json!({
                        "type":"data-awaken-file",
                        "data": {
                            "object":"awaken.file_reference",
                            "fileRef":"file_fedcba9876543210fedcba9876543210",
                            "kind":"document"
                        }
                    }),
                ],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };

        let p = process(req, &[]);
        assert!(matches!(
            &p.messages[0].content[0],
            ContentBlock::Image { source: ImageSource::File { file_id } }
                if file_id == "file_0123456789abcdef0123456789abcdef"
        ));
        assert!(matches!(
            &p.messages[0].content[1],
            ContentBlock::Document { source: DocumentSource::File { file_id }, .. }
                if file_id == "file_fedcba9876543210fedcba9876543210"
        ));
    }

    #[test]
    fn dedups_known_ids() {
        let req = AiSdkChatRequest {
            messages: vec![
                ui("user", "u1", vec![json!({"type":"text","text":"old"})]),
                ui(
                    "assistant",
                    "a1",
                    vec![json!({"type":"text","text":"prior"})],
                ),
                ui("user", "u2", vec![json!({"type":"text","text":"new"})]),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let history = [
            committed("u1", Role::User, "old"),
            committed("a1", Role::Assistant, "prior"),
        ];
        let p = process(req, &history);
        assert_eq!(p.messages.len(), 1);
        assert_eq!(blocks_text(&p.messages[0].content), "new");
    }

    #[test]
    fn retains_the_known_operation_message_for_the_durable_replay_decision() {
        // Turn classification cause/effect graph. C1 the latest stable input id
        // is absent/present in committed history; C2 older history is present;
        // C3 the replayed content is equal/different. Effects: E1 a fresh turn
        // forwards only its latest input; E2 an exact response-loss retry
        // forwards that same input and operation id to Session Run admission;
        // E3 conflicting reuse forwards the attempted bytes under the same
        // operation id so the existing reservation fingerprint rejects them;
        // E4 older history never becomes new input. The adapter must not decide
        // C3: the durable Session Run reservation is the sole equality owner.
        //
        // | Rule | latest known | older known | replay bytes | Effect |
        // | R1   | no           | yes         | n/a          | E1+E4 |
        // | R2   | yes          | any         | equal        | E2+E4 |
        // | R3   | yes          | any         | different    | E3+E4 |
        // `dedups_known_ids` owns R1; this test owns R2-R3.
        let history = [committed("u1", Role::User, "original")];
        for (rule, text) in [("R2", "original"), ("R3", "conflicting")] {
            let req = AiSdkChatRequest {
                messages: vec![ui("user", "u1", vec![json!({"type":"text","text":text})])],
                thread_id: Some("t".into()),
                agent_id: None,
            };
            let processed = process(req, &history);
            assert_eq!(processed.operation_id, "u1", "{rule}");
            assert_eq!(processed.messages.len(), 1, "{rule}");
            assert_eq!(processed.messages[0].id.0, "u1", "{rule}");
            assert_eq!(blocks_text(&processed.messages[0].content), text, "{rule}");
        }
    }

    #[test]
    fn operation_shape_decision_table_rejects_ambiguous_or_spoofed_input() {
        // Closed operation-shape graph. C1 trailing input count is 0/1/>1; C2
        // the final operation is input/tool-decision/history; C3 a wire id is absent/new/committed;
        // C4 a committed role matches/conflicts; C5 committed history ids are
        // unique/duplicated. Effects: E1 one input becomes one Run operation;
        // E2 decision-only becomes one resume operation; E3 every ambiguous,
        // spoofed, or corrupt-history shape rejects before Run/resume; E4 a
        // current input makes every earlier assistant decision historical.
        //
        // | Rule | C1 | C2 | C3/C4 | C5 | Effect |
        // | S1 | 1 | no  | new or matching | unique | E1 (sibling tests) |
        // | S2 | 0 | yes | id or absent | unique | E2 |
        // | S3 | >1 | any | any | unique | E3 cardinality |
        // | S4 | 1 | earlier decision | any | unique | E4+E1 |
        // | S5 | 1 | no | committed/conflicting | unique | E3 role |
        // | S6 | any | any | any | duplicate | E3 history corruption |
        // | S7 | 0 | no | history only | unique | E3 empty operation |
        let two_inputs = |second: &str| AiSdkChatRequest {
            messages: vec![
                ui("user", "u1", vec![json!({"type":"text","text":"one"})]),
                ui("user", "u2", vec![json!({"type":"text","text":second})]),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        assert_eq!(
            rejection(two_inputs("two"), &[]),
            ProcessError::InputCardinality,
            "S3 fresh batch"
        );
        let multi_history = [
            committed("u1", Role::User, "one"),
            committed("u2", Role::User, "two"),
        ];
        assert_eq!(
            rejection(two_inputs("two"), &multi_history),
            ProcessError::InputCardinality,
            "S3 replayed batch"
        );

        let historical_decision_then_turn = AiSdkChatRequest {
            messages: vec![
                ui(
                    "assistant",
                    "a1",
                    vec![decision_part("output-available", json!({"output":"ok"}))],
                ),
                ui("user", "u2", vec![json!({"type":"text","text":"next"})]),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let processed = process(
            historical_decision_then_turn,
            &[committed("a1", Role::Assistant, "historical tool output")],
        );
        assert_eq!(processed.messages.len(), 1, "S4/E4+E1");
        assert_eq!(processed.messages[0].id.0, "u2", "S4/E4+E1");
        assert!(processed.decisions.is_empty(), "S4/E4");

        let spoof = AiSdkChatRequest {
            messages: vec![ui(
                "user",
                "a1",
                vec![json!({"type":"text","text":"spoof"})],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        assert_eq!(
            rejection(spoof, &[committed("a1", Role::Assistant, "answer")]),
            ProcessError::CommittedRoleConflict("a1".into()),
            "S5"
        );

        let current = AiSdkChatRequest {
            messages: vec![ui("user", "u2", vec![json!({"type":"text","text":"next"})])],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let duplicate_history = [
            committed("u1", Role::User, "one"),
            committed("u1", Role::User, "one"),
        ];
        assert_eq!(
            rejection(current, &duplicate_history),
            ProcessError::DuplicateHistoryId("u1".into()),
            "S6"
        );

        let history_only = AiSdkChatRequest {
            messages: vec![
                ui("user", "u1", vec![json!({"type":"text","text":"one"})]),
                ui(
                    "assistant",
                    "a1",
                    vec![json!({"type":"text","text":"answer"})],
                ),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let history = [
            committed("u1", Role::User, "one"),
            committed("a1", Role::Assistant, "answer"),
        ];
        assert_eq!(
            rejection(history_only, &history),
            ProcessError::EmptyOperation,
            "S7"
        );

        let mut idless_decision = ui(
            "assistant",
            "ignored",
            vec![decision_part("output-available", json!({"output":"ok"}))],
        );
        idless_decision.id = None;
        let decision_only = AiSdkChatRequest {
            messages: vec![
                ui("user", "u1", vec![json!({"type":"text","text":"one"})]),
                idless_decision,
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let processed = process(decision_only, &[committed("u1", Role::User, "one")]);
        assert!(processed.is_resume_only(), "S2/E2");
        assert!(
            processed.operation_id.starts_with("ai-sdk-operation-"),
            "S2/E2 must not inherit u1: {}",
            processed.operation_id
        );
    }

    #[test]
    fn extracts_client_output_decision() {
        let req = AiSdkChatRequest {
            messages: vec![
                ui("user", "u1", vec![json!({"type":"text","text":"old"})]),
                ui(
                    "assistant",
                    "a1",
                    vec![json!({
                        "type":"tool-submit_answer",
                        "toolCallId":"c1",
                        "state":"output-available",
                        "output":"42"
                    })],
                ),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let history = [committed("u1", Role::User, "old")];
        let p = process(req, &history);
        assert!(p.is_resume_only());
        assert_eq!(p.operation_id, "a1");
        assert_eq!(p.decisions.len(), 1);
        assert_eq!(p.decisions[0].tool_call_id, "c1");
        assert_eq!(p.decisions[0].kind, DecisionKind::Output(json!("42")));
    }

    #[test]
    fn skips_provider_executed_history() {
        let req = AiSdkChatRequest {
            messages: vec![
                ui(
                    "assistant",
                    "a1",
                    vec![json!({
                        "type":"tool-read",
                        "toolCallId":"c1",
                        "state":"output-available",
                        "output":"x",
                        "providerExecuted": true
                    })],
                ),
                ui("user", "u1", vec![json!({"type":"text","text":"next"})]),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process(req, &[]);
        assert!(p.decisions.is_empty());
        assert_eq!(p.messages.len(), 1);
    }

    #[test]
    fn accepts_a_user_decision_for_a_provider_executed_approval() {
        /* Decision extraction: C1 provider-executed output is historical; C2
         * provider-executed approval-responded is a fresh human decision.
         * E1 ignore history; E2 resume the built-in permission gate.
         * R1=C1=>E1 (covered above); R2=C2=>E2. */
        let req = AiSdkChatRequest {
            messages: vec![ui(
                "assistant",
                "a1",
                vec![json!({
                    "type":"tool-bash",
                    "toolCallId":"c1",
                    "state":"approval-responded",
                    "approval":{"id":"c1","approved":true},
                    "providerExecuted":true
                })],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let processed = process(req, &[]);
        assert!(processed.is_resume_only());
        assert_eq!(processed.decisions[0].kind, DecisionKind::Approved);
    }

    fn decision_part(state: &str, extra: Value) -> Value {
        let mut part = json!({ "type": "tool-probe", "toolCallId": "c1", "state": state });
        if let (Value::Object(dst), Value::Object(src)) = (&mut part, extra) {
            dst.extend(src);
        }
        part
    }

    fn only_decision(state: &str, extra: Value) -> DecisionKind {
        let req = AiSdkChatRequest {
            messages: vec![ui("assistant", "a1", vec![decision_part(state, extra)])],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process(req, &[]);
        assert_eq!(
            p.decisions.len(),
            1,
            "exactly one decision for state {state}"
        );
        p.decisions.into_iter().next().unwrap().kind
    }

    #[test]
    fn decodes_output_error_decision() {
        assert_eq!(
            only_decision("output-error", json!({ "errorText": "boom" })),
            DecisionKind::Error("boom".into())
        );
    }

    #[test]
    fn output_error_without_text_uses_the_default_message() {
        assert_eq!(
            only_decision("output-error", json!({})),
            DecisionKind::Error("tool execution error".into())
        );
    }

    #[test]
    fn decodes_output_denied_decision() {
        assert_eq!(
            only_decision("output-denied", json!({})),
            DecisionKind::Denied
        );
    }

    #[test]
    fn approval_responded_true_is_approved() {
        assert_eq!(
            only_decision(
                "approval-responded",
                json!({ "approval": { "approved": true } })
            ),
            DecisionKind::Approved
        );
    }

    #[test]
    fn approval_responded_false_is_denied() {
        assert_eq!(
            only_decision(
                "approval-responded",
                json!({ "approval": { "approved": false } })
            ),
            DecisionKind::Denied
        );
    }

    #[test]
    fn approval_responded_without_an_approval_field_is_denied() {
        // Absent `approval` ⇒ `unwrap_or(false)` ⇒ fail closed to Denied.
        assert_eq!(
            only_decision("approval-responded", json!({})),
            DecisionKind::Denied
        );
    }

    #[test]
    fn output_available_without_output_decodes_as_null() {
        assert_eq!(
            only_decision("output-available", json!({})),
            DecisionKind::Output(Value::Null)
        );
    }

    #[test]
    fn unknown_decision_state_is_dropped() {
        let req = AiSdkChatRequest {
            messages: vec![
                ui("assistant", "a1", vec![decision_part("mystery", json!({}))]),
                ui("user", "u1", vec![json!({"type":"text","text":"next"})]),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        assert!(process(req, &[]).decisions.is_empty());
    }

    #[test]
    fn non_tool_assistant_part_yields_no_decision() {
        let req = AiSdkChatRequest {
            messages: vec![
                ui(
                    "assistant",
                    "a1",
                    vec![json!({ "type": "text", "text": "just prose" })],
                ),
                ui("user", "u1", vec![json!({"type":"text","text":"next"})]),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        assert!(process(req, &[]).decisions.is_empty());
    }

    #[test]
    fn system_role_message_converts_to_system() {
        let req = AiSdkChatRequest {
            messages: vec![ui(
                "system",
                "s1",
                vec![json!({"type":"text","text":"be terse"})],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process(req, &[]);
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].role, Role::System);
    }

    #[test]
    fn a_missing_thread_id_is_minted() {
        let req = AiSdkChatRequest {
            messages: vec![ui("user", "u1", vec![json!({"type":"text","text":"hi"})])],
            thread_id: None,
            agent_id: None,
        };
        let p = process(req, &[]);
        assert!(p.thread_id.starts_with("thread-"));
    }

    fn user_parts(parts: Vec<Value>) -> ProcessedRequest {
        let req = AiSdkChatRequest {
            messages: vec![ui("user", "u1", parts)],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        process(req, &[])
    }

    #[test]
    fn a_malformed_data_uri_falls_back_to_an_image_url() {
        use awaken_agent_contract::agent::content::ImageSource;
        let p = user_parts(vec![
            json!({"type":"file","mediaType":"image/png","url":"data:garbage-no-comma"}),
        ]);
        assert!(matches!(
            &p.messages[0].content[0],
            ContentBlock::Image { source: ImageSource::Url { url } } if url == "data:garbage-no-comma"
        ));
    }

    #[test]
    fn a_remote_image_url_becomes_an_image_url_block() {
        use awaken_agent_contract::agent::content::ImageSource;
        let p = user_parts(vec![
            json!({"type":"file","mediaType":"image/png","url":"https://x/y.png"}),
        ]);
        assert!(matches!(
            &p.messages[0].content[0],
            ContentBlock::Image { source: ImageSource::Url { url } } if url == "https://x/y.png"
        ));
    }

    #[test]
    fn an_empty_text_part_is_dropped() {
        let p = user_parts(vec![
            json!({"type":"text","text":""}),
            json!({"type":"text","text":"keep"}),
        ]);
        assert_eq!(p.messages[0].content.len(), 1);
        assert_eq!(blocks_text(&p.messages[0].content), "keep");
    }

    #[test]
    fn a_non_image_file_part_is_dropped() {
        let p = user_parts(vec![
            json!({"type":"file","mediaType":"application/pdf","url":"data:application/pdf;base64,AAAA"}),
            json!({"type":"text","text":"keep"}),
        ]);
        assert_eq!(p.messages[0].content.len(), 1);
        assert_eq!(blocks_text(&p.messages[0].content), "keep");
    }

    #[test]
    fn an_unknown_part_type_is_dropped_from_content() {
        // A non-content part (e.g. reasoning/step-start) is not user input.
        let p = user_parts(vec![
            json!({ "type": "reasoning", "text": "thinking" }),
            json!({ "type": "text", "text": "keep" }),
        ]);
        assert_eq!(p.messages[0].content.len(), 1);
        assert_eq!(blocks_text(&p.messages[0].content), "keep");
    }

    #[test]
    fn result_text_renders_the_three_value_shapes() {
        // A string is unquoted; null is empty; anything else is its JSON form.
        assert_eq!(result_text(&json!("hi")), "hi");
        assert_eq!(result_text(&Value::Null), "");
        assert_eq!(result_text(&json!({ "a": 1 })), "{\"a\":1}");
    }
}
