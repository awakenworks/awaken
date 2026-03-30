//! /v1/ag-ui routes.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};

use crate::app::AppState;
use crate::http_run::wire_sse_relay;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::routes::ApiError;
use awaken_runtime::RunRequest;

use super::encoder::AgUiEncoder;
use super::types::Role;

/// Build AG-UI routes.
pub fn ag_ui_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/ag-ui/run", post(ag_ui_run))
        .route(
            "/v1/ag-ui/threads/:thread_id/runs",
            post(ag_ui_run_threaded),
        )
        .route(
            "/v1/ag-ui/threads/:thread_id/interrupt",
            post(interrupt_thread),
        )
        .route(
            "/v1/ag-ui/agents/:agent_id/runs",
            post(ag_ui_run_agent_scoped),
        )
        .route("/v1/ag-ui/threads/:id/messages", get(thread_messages))
}

#[derive(Debug, Deserialize)]
struct AgUiResumePayload {
    #[serde(rename = "interruptId", alias = "interrupt_id")]
    interrupt_id: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AgUiRunRequest {
    #[serde(rename = "threadId", alias = "thread_id", default)]
    thread_id: Option<String>,
    #[serde(rename = "agentId", alias = "agent_id", default)]
    agent_id: Option<String>,
    #[serde(default)]
    messages: Vec<AgUiMessage>,
    #[serde(default)]
    state: Option<Value>,
    /// AG-UI `context` array — accepted as an alternative/supplement to `state`.
    #[serde(default)]
    context: Option<Value>,
    #[serde(default)]
    resume: Option<AgUiResumePayload>,
    /// Frontend tool definitions sent by CopilotKit / AG-UI clients.
    #[serde(default)]
    tools: Vec<AgUiToolDefinition>,
}

#[derive(Debug, Deserialize)]
struct AgUiToolDefinition {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<serde_json::Value>,
}

impl AgUiRunRequest {
    /// Return the effective frontend context by merging `state` and `context`.
    /// CopilotKit sends both fields; the old `alias = "context"` on `state`
    /// caused serde to reject the request as a duplicate field.
    fn into_parts(
        self,
    ) -> (
        Option<String>,
        Option<String>,
        Vec<AgUiMessage>,
        Option<Value>,
        Option<AgUiResumePayload>,
        Vec<AgUiToolDefinition>,
    ) {
        let state = match (self.state, self.context) {
            (Some(s), None) | (Some(s), Some(Value::Null)) => Some(s),
            (None, Some(c)) | (Some(Value::Null), Some(c)) => Some(c),
            (Some(Value::Object(mut s)), Some(Value::Object(c))) => {
                s.extend(c);
                Some(Value::Object(s))
            }
            (Some(s), Some(_)) => Some(s), // state wins for non-object values
            (None, None) => None,
        };
        (
            self.thread_id,
            self.agent_id,
            self.messages,
            state,
            self.resume,
            self.tools,
        )
    }
}

#[derive(Debug, Deserialize)]
struct AgUiMessage {
    role: String,
    #[serde(default)]
    content: serde_json::Value,
}

fn convert_messages(msgs: Vec<AgUiMessage>) -> Vec<Message> {
    msgs.into_iter()
        .filter_map(|m| {
            let blocks = parse_ag_ui_content(&m.content)?;
            match m.role.as_str() {
                "user" => Some(Message::user_with_content(blocks)),
                "assistant" => Some(Message::assistant(
                    awaken_contract::contract::content::extract_text(&blocks),
                )),
                "system" => Some(Message::system(
                    awaken_contract::contract::content::extract_text(&blocks),
                )),
                _ => None,
            }
        })
        .collect()
}

fn parse_ag_ui_content(
    content: &serde_json::Value,
) -> Option<Vec<awaken_contract::contract::content::ContentBlock>> {
    use awaken_contract::contract::content::ContentBlock;

    match content {
        serde_json::Value::String(s) => Some(vec![ContentBlock::text(s.as_str())]),
        serde_json::Value::Array(arr) => {
            let blocks: Vec<ContentBlock> = arr
                .iter()
                .filter_map(|v| {
                    let part: super::types::InputContentPart =
                        serde_json::from_value(v.clone()).ok()?;
                    input_part_to_block(part)
                })
                .collect();
            if blocks.is_empty() {
                None
            } else {
                Some(blocks)
            }
        }
        serde_json::Value::Null => None,
        _ => None,
    }
}

fn input_part_to_block(
    part: super::types::InputContentPart,
) -> Option<awaken_contract::contract::content::ContentBlock> {
    use super::types::{InputContentPart, InputContentSource};
    use awaken_contract::contract::content::ContentBlock;

    match part {
        InputContentPart::Text { text } => Some(ContentBlock::text(text)),
        InputContentPart::Image { source, .. } => Some(match source {
            InputContentSource::Data { value, mime_type } => {
                ContentBlock::image_base64(mime_type, value)
            }
            InputContentSource::Url { value, .. } => ContentBlock::image_url(value),
        }),
        InputContentPart::Audio { source, .. } => Some(match source {
            InputContentSource::Data { value, mime_type } => {
                ContentBlock::audio_base64(mime_type, value)
            }
            InputContentSource::Url { value, .. } => ContentBlock::audio_url(value),
        }),
        InputContentPart::Video { source, .. } => Some(match source {
            InputContentSource::Data { value, mime_type } => {
                ContentBlock::video_base64(mime_type, value)
            }
            InputContentSource::Url { value, .. } => ContentBlock::video_url(value),
        }),
        InputContentPart::Document { source, .. } => Some(match source {
            InputContentSource::Data { value, mime_type } => {
                ContentBlock::document_base64(mime_type, value, None)
            }
            InputContentSource::Url { value, .. } => ContentBlock::document_url(value, None),
        }),
    }
}

async fn ag_ui_run(
    State(st): State<AppState>,
    Json(payload): Json<AgUiRunRequest>,
) -> Result<Response, ApiError> {
    ag_ui_run_inner(st, payload).await
}

/// Thread-centric route: `POST /v1/ag-ui/threads/:thread_id/runs`
async fn ag_ui_run_threaded(
    State(st): State<AppState>,
    Path(thread_id): Path<String>,
    Json(mut payload): Json<AgUiRunRequest>,
) -> Result<Response, ApiError> {
    payload.thread_id = Some(thread_id);
    ag_ui_run_inner(st, payload).await
}

/// Agent-scoped route: `POST /v1/ag-ui/agents/:agent_id/runs`
async fn ag_ui_run_agent_scoped(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(mut payload): Json<AgUiRunRequest>,
) -> Result<Response, ApiError> {
    payload.agent_id = Some(agent_id);
    ag_ui_run_inner(st, payload).await
}

async fn interrupt_thread(
    State(st): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Response, ApiError> {
    let interrupted = st
        .mailbox
        .interrupt(&thread_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    if interrupted.active_job.is_some() || interrupted.superseded_count > 0 {
        return Ok((
            axum::http::StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": "interrupt_requested",
                "thread_id": thread_id,
                "superseded_jobs": interrupted.superseded_count,
            })),
        )
            .into_response());
    }

    Err(ApiError::ThreadNotFound(thread_id))
}

/// Convert an AG-UI resume payload into a `(tool_call_id, ToolCallResume)` pair.
fn convert_resume_to_decision(resume: AgUiResumePayload) -> Option<(String, ToolCallResume)> {
    let tool_call_id = resume.interrupt_id?;
    let payload = resume.payload.unwrap_or(Value::Null);

    let action = if payload.get("approved").and_then(Value::as_bool) == Some(false) {
        ResumeDecisionAction::Cancel
    } else {
        ResumeDecisionAction::Resume
    };

    Some((
        tool_call_id,
        ToolCallResume {
            decision_id: uuid::Uuid::now_v7().to_string(),
            action,
            result: payload,
            reason: None,
            updated_at: awaken_contract::now_ms(),
        },
    ))
}

async fn ag_ui_run_inner(st: AppState, payload: AgUiRunRequest) -> Result<Response, ApiError> {
    let (thread_id_raw, agent_id, raw_messages, state, resume, frontend_tools) =
        payload.into_parts();
    let messages = convert_messages(raw_messages);
    let (thread_id, messages) = crate::request::prepare_run_inputs(thread_id_raw, messages)?;
    let messages = crate::request::inject_frontend_context(messages, state);

    // Convert AG-UI resume payload into a decision for the runtime
    let decisions: Vec<(String, ToolCallResume)> = resume
        .and_then(convert_resume_to_decision)
        .into_iter()
        .collect();

    // Convert AG-UI frontend tool definitions into ToolDescriptor values
    let frontend_tools: Vec<awaken_contract::contract::tool::ToolDescriptor> = frontend_tools
        .into_iter()
        .map(|t| {
            awaken_contract::contract::tool::ToolDescriptor::new(
                &t.name,
                &t.name,
                t.description.as_deref().unwrap_or("Frontend tool"),
            )
            .with_parameters(
                t.parameters
                    .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
            )
        })
        .collect();

    let mut request = RunRequest::new(thread_id, messages);
    if let Some(id) = agent_id {
        request = request.with_agent_id(id);
    }
    if !frontend_tools.is_empty() {
        request = request.with_frontend_tools(frontend_tools);
    }
    if !decisions.is_empty() {
        request = request.with_decisions(decisions);
    }
    let (_result, event_rx) = st
        .mailbox
        .submit(request)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let encoder = AgUiEncoder::new();
    let sse_rx = wire_sse_relay(event_rx, encoder, st.config.sse_buffer_size, None);

    Ok(sse_response(sse_body_stream(sse_rx)))
}

async fn thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<crate::query::MessageQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let messages = st
        .store
        .load_messages(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .unwrap_or_default();

    let offset = params.offset_or_default();
    let limit = params.clamped_limit();
    let total = messages.len();

    let encoded: Vec<Value> = messages
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|m| {
            let role = match m.role {
                awaken_contract::contract::message::Role::System => Role::System,
                awaken_contract::contract::message::Role::User => Role::User,
                awaken_contract::contract::message::Role::Assistant => Role::Assistant,
                awaken_contract::contract::message::Role::Tool => Role::Tool,
            };
            serde_json::json!({
                "id": m.id,
                "role": role,
                "content": m.content,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "messages": encoded,
        "total": total,
        "has_more": offset + encoded.len() < total,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn convert_ag_ui_messages() {
        let msgs = vec![
            AgUiMessage {
                role: "user".into(),
                content: json!("hello"),
            },
            AgUiMessage {
                role: "assistant".into(),
                content: json!("hi"),
            },
            AgUiMessage {
                role: "unknown".into(),
                content: json!("x"),
            },
        ];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].text(), "hello");
        assert_eq!(converted[1].text(), "hi");
    }

    #[test]
    fn convert_empty_messages() {
        assert!(convert_messages(vec![]).is_empty());
    }

    #[test]
    fn convert_multimodal_user_message() {
        let msgs = vec![AgUiMessage {
            role: "user".into(),
            content: json!([
                {"type": "text", "text": "Look at this"},
                {"type": "image", "source": {"type": "url", "value": "https://example.com/img.png"}}
            ]),
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].content.len(), 2);
    }

    #[test]
    fn deserialize_request_with_state_and_context() {
        // CopilotKit sends both `state` and `context` in the same request.
        // Previously `context` was a serde alias for `state`, causing a
        // "duplicate field" deserialization error.
        let raw = json!({
            "threadId": "t1",
            "messages": [{"role": "user", "content": "hi"}],
            "state": {"key": "from_state"},
            "context": [{"type": "text", "text": "extra"}]
        });
        let req: AgUiRunRequest = serde_json::from_value(raw).expect("should deserialize");
        assert!(req.state.is_some());
        assert!(req.context.is_some());
    }

    #[test]
    fn into_parts_merges_objects() {
        let raw = json!({
            "threadId": "t1",
            "messages": [],
            "state": {"a": 1},
            "context": {"b": 2}
        });
        let req: AgUiRunRequest = serde_json::from_value(raw).unwrap();
        let (_, _, _, state, _, _) = req.into_parts();
        let obj = state.unwrap();
        assert_eq!(obj["a"], json!(1));
        assert_eq!(obj["b"], json!(2));
    }

    #[test]
    fn convert_resume_approved_true() {
        let resume = AgUiResumePayload {
            interrupt_id: Some("tc_1".into()),
            payload: Some(json!({"approved": true})),
        };
        let (id, tcr) = convert_resume_to_decision(resume).unwrap();
        assert_eq!(id, "tc_1");
        assert_eq!(tcr.action, ResumeDecisionAction::Resume);
        assert_eq!(tcr.result, json!({"approved": true}));
    }

    #[test]
    fn convert_resume_approved_false() {
        let resume = AgUiResumePayload {
            interrupt_id: Some("tc_2".into()),
            payload: Some(json!({"approved": false})),
        };
        let (id, tcr) = convert_resume_to_decision(resume).unwrap();
        assert_eq!(id, "tc_2");
        assert_eq!(tcr.action, ResumeDecisionAction::Cancel);
    }

    #[test]
    fn convert_resume_arbitrary_payload() {
        let resume = AgUiResumePayload {
            interrupt_id: Some("tc_3".into()),
            payload: Some(json!({"data": "some result"})),
        };
        let (id, tcr) = convert_resume_to_decision(resume).unwrap();
        assert_eq!(id, "tc_3");
        assert_eq!(tcr.action, ResumeDecisionAction::Resume);
        assert_eq!(tcr.result, json!({"data": "some result"}));
    }

    #[test]
    fn convert_resume_no_interrupt_id_returns_none() {
        let resume = AgUiResumePayload {
            interrupt_id: None,
            payload: Some(json!({"approved": true})),
        };
        assert!(convert_resume_to_decision(resume).is_none());
    }

    #[test]
    fn convert_resume_no_payload() {
        let resume = AgUiResumePayload {
            interrupt_id: Some("tc_4".into()),
            payload: None,
        };
        let (id, tcr) = convert_resume_to_decision(resume).unwrap();
        assert_eq!(id, "tc_4");
        assert_eq!(tcr.action, ResumeDecisionAction::Resume);
        assert_eq!(tcr.result, Value::Null);
    }

    #[test]
    fn into_parts_context_only() {
        let raw = json!({
            "messages": [],
            "context": [{"type": "text"}]
        });
        let req: AgUiRunRequest = serde_json::from_value(raw).unwrap();
        let (_, _, _, state, _, _) = req.into_parts();
        assert!(state.is_some());
    }
}
