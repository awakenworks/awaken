use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use tirea_contract::io::decision_translation::suspension_response_to_decision;
use tirea_contract::{Message, RunOrigin, RunRequest, SuspensionResponse, ToolCallDecision};

use crate::message::{ToolState, ToolUIPart};

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "AiSdkV6MessagesRunRequest")]
pub struct AiSdkV6RunRequest {
    pub thread_id: String,
    pub input: String,
    pub parent_thread_id: Option<String>,
    pub trigger: Option<AiSdkTrigger>,
    pub message_id: Option<String>,
    interaction_responses: Vec<SuspensionResponse>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AiSdkTrigger {
    SubmitMessage,
    RegenerateMessage,
}

#[derive(Debug, Clone, Deserialize)]
struct AiSdkV6MessagesRunRequest {
    #[serde(default)]
    id: Option<String>,
    // Legacy fields are rejected for AI SDK v6 UI transport.
    #[serde(rename = "sessionId", default)]
    legacy_session_id: Option<String>,
    #[serde(rename = "input", default)]
    legacy_input: Option<String>,
    #[serde(default)]
    messages: Vec<Value>,
    #[serde(rename = "parentThreadId", alias = "parent_thread_id", default)]
    parent_thread_id: Option<String>,
    #[serde(default)]
    trigger: Option<AiSdkTrigger>,
    #[serde(default)]
    #[serde(rename = "messageId")]
    message_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolApprovalResponsePart {
    #[serde(rename = "approvalId")]
    approval_id: String,
    #[serde(default)]
    approved: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
}

impl TryFrom<AiSdkV6MessagesRunRequest> for AiSdkV6RunRequest {
    type Error = String;

    fn try_from(req: AiSdkV6MessagesRunRequest) -> Result<Self, Self::Error> {
        if req.legacy_session_id.is_some() || req.legacy_input.is_some() {
            return Err(
                "legacy AI SDK payload shape is no longer supported; use id/messages".to_string(),
            );
        }

        let thread_id = req.id.unwrap_or_default();
        let input = extract_last_user_text(&req.messages).unwrap_or_default();
        let interaction_responses = extract_interaction_responses(&req.messages);
        Ok(Self {
            thread_id,
            input,
            parent_thread_id: req.parent_thread_id,
            trigger: req.trigger,
            message_id: req.message_id,
            interaction_responses,
        })
    }
}

impl AiSdkV6RunRequest {
    /// Build a request from explicit thread/input values (non-UI transport path).
    pub fn from_thread_input(thread_id: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            input: input.into(),
            parent_thread_id: None,
            trigger: Some(AiSdkTrigger::SubmitMessage),
            message_id: None,
            interaction_responses: Vec::new(),
        }
    }

    /// Validate the request before processing.
    ///
    /// Checks that:
    /// - `thread_id` is non-empty
    /// - `messageId` is present and non-empty for `regenerate-message`
    /// - At least one of user input, suspension decisions, or regenerate trigger is present
    pub fn validate(&self) -> Result<(), String> {
        if self.thread_id.trim().is_empty() {
            return Err("id cannot be empty".into());
        }
        if self.trigger == Some(AiSdkTrigger::RegenerateMessage) {
            match self.message_id.as_deref() {
                None => {
                    return Err("messageId is required for regenerate-message".into());
                }
                Some(id) if id.trim().is_empty() => {
                    return Err("messageId cannot be empty for regenerate-message".into());
                }
                _ => {}
            }
        }
        let is_regenerate = self.trigger == Some(AiSdkTrigger::RegenerateMessage);
        if !self.has_user_input() && !self.has_suspension_decisions() && !is_regenerate {
            return Err("request must include user input or suspension decisions".into());
        }
        Ok(())
    }

    /// Whether the incoming request includes a non-empty user input message.
    pub fn has_user_input(&self) -> bool {
        !self.input.trim().is_empty()
    }

    /// Whether the incoming request includes any interaction responses.
    pub fn has_interaction_responses(&self) -> bool {
        !self.interaction_responses.is_empty()
    }

    /// Whether the incoming request includes any suspension decisions.
    pub fn has_suspension_decisions(&self) -> bool {
        !self.suspension_decisions().is_empty()
    }

    /// Suspension responses extracted from incoming UI messages.
    pub fn interaction_responses(&self) -> Vec<SuspensionResponse> {
        self.interaction_responses.clone()
    }

    /// Suspension decisions extracted from incoming UI messages.
    pub fn suspension_decisions(&self) -> Vec<ToolCallDecision> {
        self.interaction_responses()
            .into_iter()
            .map(suspension_response_to_decision)
            .collect()
    }

    /// Convert this AI SDK request to the internal runtime request.
    ///
    /// Mapping rules:
    /// - `thread_id` is treated as optional when blank/whitespace.
    /// - `run_id` is always server-assigned (not client-supplied).
    /// - Only extracted user input text is appended as runtime user message.
    /// - `state`, `parent_run_id`, and `resource_id` are not supplied by AI SDK v6 input.
    pub fn into_runtime_run_request(self, agent_id: String) -> RunRequest {
        let initial_decisions = self.suspension_decisions();
        let mut messages = Vec::new();
        if self.has_user_input() {
            messages.push(Message::user(self.input));
        }
        RunRequest {
            agent_id,
            thread_id: if self.thread_id.trim().is_empty() {
                None
            } else {
                Some(self.thread_id)
            },
            run_id: None,
            parent_run_id: None,
            parent_thread_id: self.parent_thread_id,
            resource_id: None,
            origin: RunOrigin::AiSdk,
            state: None,
            messages,
            initial_decisions,
            source_mailbox_entry_id: None,
        }
    }
}

fn extract_last_user_text(messages: &[Value]) -> Option<String> {
    for message in messages.iter().rev() {
        let Some(role) = message_role(message) else {
            continue;
        };
        if !role.eq_ignore_ascii_case("user") {
            continue;
        }

        if let Some(content) = message_content_string(message) {
            return Some(content.to_string());
        }

        let text = extract_text_from_parts(&message_parts(message));
        if !text.is_empty() {
            return Some(text);
        }
    }

    None
}

fn extract_interaction_responses(messages: &[Value]) -> Vec<SuspensionResponse> {
    let mut latest_by_id: HashMap<String, (usize, Value)> = HashMap::new();
    let mut ordinal = 0usize;

    for message in messages {
        let Some(role) = message_role(message) else {
            continue;
        };
        if !role.eq_ignore_ascii_case("assistant") {
            continue;
        }

        for part in message_parts(message) {
            if let Some((target_id, result)) = parse_interaction_response_part(&part) {
                latest_by_id.insert(target_id, (ordinal, result));
                ordinal += 1;
            }
        }
    }

    let mut responses: Vec<(usize, SuspensionResponse)> = latest_by_id
        .into_iter()
        .map(|(target_id, (idx, result))| (idx, SuspensionResponse::new(target_id, result)))
        .collect();
    responses.sort_by_key(|(idx, _)| *idx);
    responses
        .into_iter()
        .map(|(_, response)| response)
        .collect()
}

fn message_role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

fn message_content_string(message: &Value) -> Option<&str> {
    message.get("content").and_then(Value::as_str)
}

fn message_parts(message: &Value) -> Vec<Value> {
    if let Some(parts) = message.get("parts").and_then(Value::as_array) {
        return parts.clone();
    }
    if let Some(parts) = message.get("content").and_then(Value::as_array) {
        return parts.clone();
    }
    Vec::new()
}

fn parse_interaction_response_part(part: &Value) -> Option<(String, Value)> {
    if part.get("type").and_then(Value::as_str) == Some("tool-approval-response") {
        return parse_tool_approval_response_part(part);
    }
    if part.get("state").and_then(Value::as_str) == Some("approval-responded") {
        return parse_approval_responded_part(part);
    }

    let tool_part = parse_tool_ui_part(part)?;
    let tool_call_id = tool_part.tool_call_id.clone();

    match tool_part.state {
        ToolState::ApprovalResponded => None,
        ToolState::OutputAvailable => Some((tool_call_id, tool_part.output.unwrap_or(Value::Null))),
        ToolState::OutputDenied => Some((tool_call_id, Value::Bool(false))),
        ToolState::OutputError => {
            let error = tool_part
                .error_text
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("tool output error");
            Some((
                tool_call_id,
                serde_json::json!({
                    "approved": false,
                    "error": error,
                }),
            ))
        }
        _ => None,
    }
}

fn parse_tool_approval_response_part(part: &Value) -> Option<(String, Value)> {
    let payload: ToolApprovalResponsePart = serde_json::from_value(part.clone()).ok()?;
    Some((
        payload.approval_id,
        approval_response_value(payload.approved.unwrap_or(false), payload.reason),
    ))
}

fn parse_approval_responded_part(part: &Value) -> Option<(String, Value)> {
    let tool_call_id = part
        .get("toolCallId")
        .or_else(|| part.get("tool_call_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let approval = part.get("approval");
    let target_id = approval
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(tool_call_id)?;
    let approved = approval
        .and_then(|v| v.get("approved"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reason = approval
        .and_then(|v| v.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((target_id, approval_response_value(approved, reason)))
}

fn parse_tool_ui_part(part: &Value) -> Option<ToolUIPart> {
    let mut normalized = part.clone();
    let map = normalized.as_object_mut()?;
    if !map.contains_key("toolCallId") {
        if let Some(tool_call_id) = map.get("tool_call_id").cloned() {
            map.insert("toolCallId".to_string(), tool_call_id);
        }
    }
    serde_json::from_value(normalized).ok()
}

fn approval_response_value(approved: bool, reason: Option<String>) -> Value {
    let mut result = serde_json::Map::new();
    result.insert("approved".to_string(), Value::Bool(approved));
    if let Some(reason) = reason {
        result.insert("reason".to_string(), Value::String(reason));
    }
    Value::Object(result)
}

fn extract_text_from_parts(parts: &[Value]) -> String {
    let mut text = String::new();
    for part in parts {
        let Some(part_type) = part.get("type").and_then(Value::as_str) else {
            continue;
        };
        if part_type != "text" {
            continue;
        }
        if let Some(segment) = part.get("text").and_then(Value::as_str) {
            text.push_str(segment);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_legacy_request_shape() {
        let err = serde_json::from_value::<AiSdkV6RunRequest>(json!({
            "sessionId": "thread-1",
            "input": "hello",
            "runId": "run-1"
        }))
        .expect_err("legacy payload must be rejected");
        assert!(
            err.to_string()
                .contains("legacy AI SDK payload shape is no longer supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deserializes_messages_request_shape_using_last_user_text() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "thread-from-id",
            "trigger": "submit-message",
            "messageId": "msg_user_2",
            "parentThreadId": "parent-thread-1",
            "messages": [
                { "id": "msg_user_1", "role": "user", "parts": [{ "type": "text", "text": "first" }] },
                { "role": "assistant", "parts": [{ "type": "text", "text": "ignored" }] },
                { "id": "msg_user_2", "role": "user", "parts": [{ "type": "text", "text": "final" }, { "type": "file", "url": "u" }] }
            ],
        }))
        .expect("messages payload should deserialize");

        assert_eq!(req.thread_id, "thread-from-id");
        assert_eq!(req.input, "final");
        assert_eq!(req.parent_thread_id.as_deref(), Some("parent-thread-1"));
        assert_eq!(req.trigger, Some(AiSdkTrigger::SubmitMessage));
        assert_eq!(req.message_id.as_deref(), Some("msg_user_2"));
    }

    #[test]
    fn into_runtime_run_request_forwards_parent_thread_id() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "thread-forward-parent",
            "parentThreadId": "p-thread",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .expect("messages payload should deserialize");

        let run_request = req.into_runtime_run_request("agent".to_string());
        assert_eq!(run_request.parent_thread_id.as_deref(), Some("p-thread"));
    }

    #[test]
    fn id_is_used_as_thread_id_in_messages_shape() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "thread-id",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .expect("messages payload should deserialize");

        assert_eq!(req.thread_id, "thread-id");
        assert_eq!(req.input, "hello");
    }

    #[test]
    fn missing_user_text_in_messages_shape_defaults_to_empty_input() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "thread-1",
            "messages": [{ "role": "assistant", "content": "no-user" }]
        }))
        .expect("messages payload should deserialize");

        assert_eq!(req.thread_id, "thread-1");
        assert_eq!(req.input, "");
    }

    #[test]
    fn extracts_approval_responded_parts_as_interaction_responses() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t1",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-echo",
                        "toolCallId": "call_echo_1",
                        "state": "approval-responded",
                        "approval": {
                            "id": "fc_perm_1",
                            "approved": true,
                            "reason": "looks safe"
                        }
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        let responses = req.interaction_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].target_id, "fc_perm_1");
        assert_eq!(responses[0].result["approved"], true);
        assert_eq!(responses[0].result["reason"], "looks safe");
    }

    #[test]
    fn extracts_tool_approval_response_parts_as_interaction_responses() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t1b",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-approval-response",
                        "approvalId": "fc_perm_7",
                        "approved": false,
                        "reason": "denied by user"
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        let responses = req.interaction_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].target_id, "fc_perm_7");
        assert_eq!(responses[0].result["approved"], false);
        assert_eq!(responses[0].result["reason"], "denied by user");
    }

    #[test]
    fn extracts_output_available_parts_as_interaction_responses() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t2",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-askUserQuestion",
                        "toolCallId": "ask_call_1",
                        "state": "output-available",
                        "output": {"answer":"blue"}
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        let responses = req.interaction_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].target_id, "ask_call_1");
        assert_eq!(responses[0].result["answer"], "blue");
    }

    #[test]
    fn output_denied_part_maps_to_denied_response() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t3",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "dynamic-tool",
                        "toolCallId": "call_1",
                        "state": "output-denied"
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        let responses = req.interaction_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].target_id, "call_1");
        assert_eq!(responses[0].result, Value::Bool(false));
    }

    #[test]
    fn output_error_part_maps_to_error_response() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t4",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "dynamic-tool",
                        "toolCallId": "call_err_1",
                        "state": "output-error",
                        "errorText": "frontend failed"
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        let responses = req.interaction_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].target_id, "call_err_1");
        assert_eq!(responses[0].result["approved"], false);
        assert_eq!(responses[0].result["error"], "frontend failed");
    }

    #[test]
    fn output_error_without_error_text_uses_default_message() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t4b",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "dynamic-tool",
                        "toolCallId": "call_err_default",
                        "state": "output-error"
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        let responses = req.interaction_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].target_id, "call_err_default");
        assert_eq!(responses[0].result["approved"], false);
        assert_eq!(responses[0].result["error"], "tool output error");
    }

    #[test]
    fn approval_responded_without_approval_id_falls_back_to_tool_call_id() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t5",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-echo",
                        "toolCallId": "fc_perm_fallback",
                        "state": "approval-responded",
                        "approval": {
                            "approved": true
                        }
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        let responses = req.interaction_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].target_id, "fc_perm_fallback");
        assert_eq!(responses[0].result["approved"], true);
    }

    #[test]
    fn tool_approval_response_without_reason_only_contains_approved_field() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t5b",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-approval-response",
                        "approvalId": "fc_perm_10",
                        "approved": true
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        let responses = req.interaction_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].target_id, "fc_perm_10");
        assert_eq!(responses[0].result["approved"], true);
        assert!(responses[0].result.get("reason").is_none());
    }

    #[test]
    fn latest_interaction_response_wins_for_same_target_id() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t6",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-PermissionConfirm",
                        "toolCallId": "fc_perm_9",
                        "state": "approval-responded",
                        "approval": {
                            "id": "fc_perm_9",
                            "approved": true
                        }
                    }]
                },
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-PermissionConfirm",
                        "toolCallId": "fc_perm_9",
                        "state": "approval-responded",
                        "approval": {
                            "id": "fc_perm_9",
                            "approved": false,
                            "reason": "user changed mind"
                        }
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        let responses = req.interaction_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].target_id, "fc_perm_9");
        assert_eq!(responses[0].result["approved"], false);
        assert_eq!(responses[0].result["reason"], "user changed mind");
    }

    #[test]
    fn suspension_decisions_preserve_last_write_order() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t6b",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-approval-response",
                        "approvalId": "perm_1",
                        "approved": true
                    }]
                },
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-approval-response",
                        "approvalId": "perm_2",
                        "approved": true
                    }]
                },
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-approval-response",
                        "approvalId": "perm_1",
                        "approved": false
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        let run_request = req.into_runtime_run_request("agent".to_string());
        let decision_targets: Vec<&str> = run_request
            .initial_decisions
            .iter()
            .map(|decision| decision.target_id.as_str())
            .collect();
        assert_eq!(
            decision_targets,
            vec!["perm_2", "perm_1"],
            "last-write ordering should be stable after dedup"
        );
    }

    #[test]
    fn interaction_only_messages_generate_empty_run_messages() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "thread-int-only",
            "messages": [
                {
                    "role": "assistant",
                    "parts": [{
                        "type": "tool-askUserQuestion",
                        "toolCallId": "ask_1",
                        "state": "output-available",
                        "output": {"message":"blue"}
                    }]
                }
            ]
        }))
        .expect("messages payload should deserialize");

        assert!(!req.has_user_input());
        assert!(req.has_interaction_responses());
        assert!(req.has_suspension_decisions());
        let decisions = req.suspension_decisions();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].target_id, "ask_1");
        let run_request = req.into_runtime_run_request("agent".to_string());
        assert!(run_request.messages.is_empty());
        assert_eq!(run_request.initial_decisions.len(), 1);
        assert_eq!(run_request.initial_decisions[0].target_id, "ask_1");
    }

    #[test]
    fn validate_rejects_empty_thread_id() {
        let req = AiSdkV6RunRequest::from_thread_input("", "hello");
        let err = req.validate().unwrap_err();
        assert!(err.contains("id cannot be empty"), "unexpected: {err}");
    }

    #[test]
    fn validate_rejects_whitespace_thread_id() {
        let req = AiSdkV6RunRequest::from_thread_input("  ", "hello");
        assert!(req.validate().is_err());
    }

    #[test]
    fn validate_rejects_regenerate_without_message_id() {
        let mut req = AiSdkV6RunRequest::from_thread_input("t1", "");
        req.trigger = Some(AiSdkTrigger::RegenerateMessage);
        req.message_id = None;
        let err = req.validate().unwrap_err();
        assert!(err.contains("messageId is required"), "unexpected: {err}");
    }

    #[test]
    fn validate_rejects_regenerate_with_empty_message_id() {
        let mut req = AiSdkV6RunRequest::from_thread_input("t1", "");
        req.trigger = Some(AiSdkTrigger::RegenerateMessage);
        req.message_id = Some("  ".to_string());
        let err = req.validate().unwrap_err();
        assert!(
            err.contains("messageId cannot be empty"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn validate_rejects_no_input_no_decisions() {
        let req = AiSdkV6RunRequest::from_thread_input("t1", "");
        let err = req.validate().unwrap_err();
        assert!(err.contains("must include user input"), "unexpected: {err}");
    }

    #[test]
    fn validate_accepts_regenerate_without_user_input() {
        let mut req = AiSdkV6RunRequest::from_thread_input("t1", "");
        req.trigger = Some(AiSdkTrigger::RegenerateMessage);
        req.message_id = Some("msg_1".to_string());
        assert!(req.validate().is_ok());
    }

    #[test]
    fn validate_accepts_valid_request() {
        let req = AiSdkV6RunRequest::from_thread_input("t1", "hello");
        assert!(req.validate().is_ok());
    }

    #[test]
    fn decision_action_null_defaults_to_cancel() {
        use tirea_contract::io::decision_translation::decision_action_from_result;
        use tirea_contract::io::ResumeDecisionAction;
        assert_eq!(
            decision_action_from_result(&Value::Null),
            ResumeDecisionAction::Cancel
        );
    }

    #[test]
    fn decision_action_array_defaults_to_cancel() {
        use tirea_contract::io::decision_translation::decision_action_from_result;
        use tirea_contract::io::ResumeDecisionAction;
        assert_eq!(
            decision_action_from_result(&json!([])),
            ResumeDecisionAction::Cancel
        );
    }

    #[test]
    fn decision_action_number_defaults_to_cancel() {
        use tirea_contract::io::decision_translation::decision_action_from_result;
        use tirea_contract::io::ResumeDecisionAction;
        assert_eq!(
            decision_action_from_result(&json!(42)),
            ResumeDecisionAction::Cancel
        );
    }
}
