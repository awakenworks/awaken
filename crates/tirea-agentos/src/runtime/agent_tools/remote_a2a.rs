use super::*;
use crate::composition::{A2aAgentBinding, RemoteSecurityConfig};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone)]
pub(super) struct A2aStartResponse {
    pub(super) context_id: Option<String>,
    pub(super) task_id: String,
}

#[derive(Debug, Clone)]
pub(super) struct A2aTaskSnapshot {
    pub(super) status: SubAgentStatus,
    pub(super) error: Option<String>,
    pub(super) done: bool,
    pub(super) output_text: Option<String>,
    pub(super) raw_task: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct A2aTaskResponse {
    #[serde(default)]
    context_id: Option<String>,
    status: String,
    #[serde(default)]
    termination_code: Option<String>,
    #[serde(default)]
    termination_detail: Option<String>,
    #[serde(default)]
    message: Option<Value>,
    #[serde(default)]
    history: Vec<Value>,
    #[serde(default)]
    artifacts: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct A2aSubmissionResponse {
    #[serde(default)]
    context_id: Option<String>,
    task_id: String,
}

#[derive(Debug, thiserror::Error)]
enum A2aClientError {
    #[error("remote A2A agents require exactly one non-empty user prompt")]
    InvalidPromptCardinality,
    #[error("remote A2A agents currently support user prompts only")]
    UnsupportedMessageRole,
    #[error("failed to submit remote A2A run: {source}")]
    SubmitRequest {
        #[source]
        source: reqwest::Error,
    },
    #[error("remote A2A submission rejected: {source}")]
    SubmitRejected {
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to decode remote A2A submission response: {source}")]
    SubmitDecode {
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to query remote A2A task status: {source}")]
    QueryRequest {
        #[source]
        source: reqwest::Error,
    },
    #[error("remote A2A task query rejected: {source}")]
    QueryRejected {
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to decode remote A2A task status: {source}")]
    QueryDecode {
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to cancel remote A2A task: {source}")]
    CancelRequest {
        #[source]
        source: reqwest::Error,
    },
    #[error("remote A2A cancel rejected: {source}")]
    CancelRejected {
        #[source]
        source: reqwest::Error,
    },
}

#[derive(Debug, Clone)]
struct A2aRuntimeClient {
    http: reqwest::Client,
}

impl A2aRuntimeClient {
    fn shared() -> Self {
        static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        Self {
            http: HTTP_CLIENT.get_or_init(reqwest::Client::new).clone(),
        }
    }

    fn authorized_request(
        &self,
        method: Method,
        url: &str,
        auth: Option<&RemoteSecurityConfig>,
    ) -> reqwest::RequestBuilder {
        authorized(&self.http, method, url, auth)
    }

    async fn submit_run(
        &self,
        target: &A2aAgentBinding,
        messages: &[Message],
    ) -> Result<A2aStartResponse, A2aClientError> {
        let input = single_user_input(messages)?;
        let url = format!("{}/message:send", agent_path(target));
        let response = self
            .authorized_request(Method::POST, &url, target.auth.as_ref())
            .json(&json!({ "input": input }))
            .send()
            .await
            .map_err(|source| A2aClientError::SubmitRequest { source })?;
        let response = response
            .error_for_status()
            .map_err(|source| A2aClientError::SubmitRejected { source })?;
        let payload = response
            .json::<A2aSubmissionResponse>()
            .await
            .map_err(|source| A2aClientError::SubmitDecode { source })?;
        Ok(A2aStartResponse {
            context_id: payload.context_id,
            task_id: payload.task_id,
        })
    }

    async fn fetch_task_snapshot(
        &self,
        target: &A2aAgentBinding,
        task_id: &str,
    ) -> Result<A2aTaskSnapshot, A2aClientError> {
        let url = format!("{}/tasks/{}", agent_path(target), task_id.trim());
        let response = self
            .authorized_request(Method::GET, &url, target.auth.as_ref())
            .send()
            .await
            .map_err(|source| A2aClientError::QueryRequest { source })?;
        let response = response
            .error_for_status()
            .map_err(|source| A2aClientError::QueryRejected { source })?;
        let payload = response
            .json::<A2aTaskResponse>()
            .await
            .map_err(|source| A2aClientError::QueryDecode { source })?;
        let _ = payload.context_id.as_deref();
        Ok(map_task_status(payload))
    }

    async fn cancel_run(
        &self,
        target: &A2aAgentBinding,
        task_id: &str,
    ) -> Result<(), A2aClientError> {
        let url = format!("{}/tasks/{}:cancel", agent_path(target), task_id.trim());
        self.authorized_request(Method::POST, &url, target.auth.as_ref())
            .json(&json!({}))
            .send()
            .await
            .map_err(|source| A2aClientError::CancelRequest { source })?
            .error_for_status()
            .map_err(|source| A2aClientError::CancelRejected { source })?;
        Ok(())
    }
}

fn base_url(target: &A2aAgentBinding) -> &str {
    target.base_url.trim_end_matches('/')
}

fn agent_path(target: &A2aAgentBinding) -> String {
    format!(
        "{}/agents/{}",
        base_url(target),
        target.remote_agent_id.trim()
    )
}

fn authorized(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    auth: Option<&RemoteSecurityConfig>,
) -> reqwest::RequestBuilder {
    let request = client.request(method, url);
    match auth {
        Some(RemoteSecurityConfig::BearerToken(token)) => request.bearer_auth(token),
        Some(RemoteSecurityConfig::Header { name, value }) => request.header(name, value),
        None => request,
    }
}

fn single_user_input(messages: &[Message]) -> Result<String, A2aClientError> {
    let mut non_empty = messages
        .iter()
        .filter(|message| !message.content.trim().is_empty())
        .collect::<Vec<_>>();
    if non_empty.len() != 1 {
        return Err(A2aClientError::InvalidPromptCardinality);
    }
    let message = non_empty.pop().expect("single message must exist");
    if message.role != Role::User {
        return Err(A2aClientError::UnsupportedMessageRole);
    }
    Ok(message.content.trim().to_string())
}

fn map_task_status(response: A2aTaskResponse) -> A2aTaskSnapshot {
    let output_text = extract_task_output_text(&response);
    let raw_task = serde_json::to_value(&response).unwrap_or(Value::Null);
    let status = response.status.trim().to_ascii_lowercase();
    match status.as_str() {
        "done" | "completed" => {
            let termination = response
                .termination_code
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase);
            match termination.as_deref() {
                Some("cancelled") | Some("cancel_requested") => A2aTaskSnapshot {
                    status: SubAgentStatus::Stopped,
                    error: response.termination_detail,
                    done: true,
                    output_text,
                    raw_task,
                },
                Some("error") => A2aTaskSnapshot {
                    status: SubAgentStatus::Failed,
                    error: response
                        .termination_detail
                        .or_else(|| Some("remote run failed".into())),
                    done: true,
                    output_text,
                    raw_task,
                },
                _ => A2aTaskSnapshot {
                    status: SubAgentStatus::Completed,
                    error: response.termination_detail,
                    done: true,
                    output_text,
                    raw_task,
                },
            }
        }
        "failed" | "error" => A2aTaskSnapshot {
            status: SubAgentStatus::Failed,
            error: response
                .termination_detail
                .or_else(|| Some("remote run failed".into())),
            done: true,
            output_text,
            raw_task,
        },
        "cancelled" | "stopped" => A2aTaskSnapshot {
            status: SubAgentStatus::Stopped,
            error: response.termination_detail,
            done: true,
            output_text,
            raw_task,
        },
        _ => A2aTaskSnapshot {
            status: SubAgentStatus::Running,
            error: None,
            done: false,
            output_text,
            raw_task,
        },
    }
}

fn push_trimmed_text(out: &mut Vec<String>, text: &str) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
}

fn extract_text_parts(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Null => {}
        Value::Bool(_) | Value::Number(_) => out.push(value.to_string()),
        Value::String(text) => push_trimmed_text(out, text),
        Value::Array(items) => {
            for item in items {
                extract_text_parts(item, out);
            }
        }
        Value::Object(object) => {
            if let Some(role) = object
                .get("role")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_ascii_lowercase)
            {
                if role != "assistant" && role != "agent" {
                    return;
                }
            }

            if let Some(text) = object.get("text").and_then(Value::as_str) {
                push_trimmed_text(out, text);
                return;
            }
            if let Some(content) = object.get("content") {
                extract_text_parts(content, out);
                return;
            }
            if let Some(parts) = object.get("parts") {
                extract_text_parts(parts, out);
                return;
            }
            if let Some(data) = object.get("data") {
                match data {
                    Value::String(text) => push_trimmed_text(out, text),
                    Value::Null => {}
                    _ => out.push(data.to_string()),
                }
                return;
            }
            if let Some(message) = object.get("message") {
                extract_text_parts(message, out);
                return;
            }
            if let Some(artifact) = object.get("artifact") {
                extract_text_parts(artifact, out);
            }
        }
    }
}

fn extract_task_output_text(response: &A2aTaskResponse) -> Option<String> {
    let mut parts = Vec::new();
    for artifact in &response.artifacts {
        extract_text_parts(artifact, &mut parts);
    }
    if parts.is_empty() {
        if let Some(message) = response.message.as_ref() {
            extract_text_parts(message, &mut parts);
        }
    }
    if parts.is_empty() {
        for message in response.history.iter().rev() {
            extract_text_parts(message, &mut parts);
            if !parts.is_empty() {
                break;
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

pub(super) async fn submit_a2a_run(
    target: &A2aAgentBinding,
    messages: &[Message],
) -> Result<A2aStartResponse, String> {
    A2aRuntimeClient::shared()
        .submit_run(target, messages)
        .await
        .map_err(|err| err.to_string())
}

pub(super) async fn fetch_a2a_task_snapshot(
    target: &A2aAgentBinding,
    task_id: &str,
) -> Result<A2aTaskSnapshot, String> {
    A2aRuntimeClient::shared()
        .fetch_task_snapshot(target, task_id)
        .await
        .map_err(|err| err.to_string())
}

pub(super) async fn poll_a2a_run_to_completion(
    target: &A2aAgentBinding,
    task_id: &str,
) -> A2aTaskSnapshot {
    loop {
        match fetch_a2a_task_snapshot(target, task_id).await {
            Ok(snapshot) if snapshot.done => return snapshot,
            Ok(_) => {
                sleep(Duration::from_millis(target.poll_interval_ms)).await;
            }
            Err(err) => {
                return A2aTaskSnapshot {
                    status: SubAgentStatus::Failed,
                    error: Some(err),
                    done: true,
                    output_text: None,
                    raw_task: Value::Null,
                }
            }
        }
    }
}

pub(super) async fn cancel_a2a_run(target: &A2aAgentBinding, task_id: &str) -> Result<(), String> {
    A2aRuntimeClient::shared()
        .cancel_run(target, task_id)
        .await
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_user_input_requires_exactly_one_user_message() {
        let result = single_user_input(&[Message::user("one"), Message::user("two")]);
        assert!(result.is_err());
    }

    #[test]
    fn single_user_input_rejects_non_user_role() {
        let result = single_user_input(&[Message::assistant("hello")]);
        assert!(matches!(
            result,
            Err(A2aClientError::UnsupportedMessageRole)
        ));
    }

    #[test]
    fn map_task_status_uses_done_and_termination_code() {
        let snapshot = map_task_status(A2aTaskResponse {
            context_id: Some("ctx-1".to_string()),
            status: "done".to_string(),
            termination_code: Some("cancelled".to_string()),
            termination_detail: None,
            message: None,
            history: Vec::new(),
            artifacts: Vec::new(),
        });
        assert_eq!(snapshot.status, SubAgentStatus::Stopped);
        assert!(snapshot.done);
        assert_eq!(snapshot.raw_task["contextId"], json!("ctx-1"));
    }

    #[test]
    fn map_task_status_extracts_output_from_artifacts_then_message() {
        let snapshot = map_task_status(A2aTaskResponse {
            context_id: Some("ctx-2".to_string()),
            status: "completed".to_string(),
            termination_code: None,
            termination_detail: None,
            message: Some(json!({"role":"assistant","content":"fallback"})),
            history: Vec::new(),
            artifacts: vec![json!({
                "parts": [
                    {"text": "hello"},
                    {"text": "world"}
                ]
            })],
        });
        assert_eq!(snapshot.output_text.as_deref(), Some("hello\n\nworld"));
        assert_eq!(
            snapshot.raw_task["artifacts"][0]["parts"][0]["text"],
            json!("hello")
        );
    }
}
