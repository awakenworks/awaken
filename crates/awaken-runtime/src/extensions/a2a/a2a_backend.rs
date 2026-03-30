//! Remote A2A agent delegation backend -- HTTP client for A2A protocol.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::message::{Message, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::backend::{AgentBackend, AgentBackendError, DelegateRunResult, DelegateRunStatus};

/// Configuration for a remote A2A agent endpoint.
#[derive(Debug, Clone)]
pub struct A2aConfig {
    /// Base URL of the remote A2A server (e.g. "https://api.example.com").
    pub base_url: String,
    /// Optional bearer token for authentication.
    pub bearer_token: Option<String>,
    /// Target agent ID on the remote server. If `None`, omits agentId in the
    /// A2A request and lets the remote server use its default agent.
    pub target_agent_id: Option<String>,
    /// Interval between poll requests.
    pub poll_interval: Duration,
    /// Maximum time to wait for task completion.
    pub timeout: Duration,
}

impl A2aConfig {
    /// Create a new A2A config with defaults for poll interval and timeout.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer_token: None,
            target_agent_id: None,
            poll_interval: Duration::from_millis(2000),
            timeout: Duration::from_secs(300),
        }
    }

    #[must_use]
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    #[must_use]
    pub fn with_target_agent_id(mut self, id: impl Into<String>) -> Self {
        self.target_agent_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Backend that delegates to a remote agent via A2A HTTP protocol.
pub struct A2aBackend {
    config: A2aConfig,
    client: reqwest::Client,
}

impl A2aBackend {
    /// Create a new A2A backend with the given configuration.
    pub fn new(config: A2aConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// Build a request with optional bearer token.
    fn build_request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let builder = self.client.request(method, url);
        match &self.config.bearer_token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// Submit a task to the remote A2A endpoint.
    async fn submit_task(&self, prompt: &str) -> Result<A2aSubmissionResponse, AgentBackendError> {
        let url = format!(
            "{}/v1/a2a/tasks/send",
            self.config.base_url.trim_end_matches('/')
        );

        let mut body = serde_json::json!({
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": prompt}]
            }
        });
        if let Some(ref target_id) = self.config.target_agent_id {
            body["agentId"] = serde_json::Value::String(target_id.clone());
        }

        let response = self
            .build_request(reqwest::Method::POST, &url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                AgentBackendError::RemoteError(format!("failed to submit A2A task: {e}"))
            })?;

        let response = response
            .error_for_status()
            .map_err(|e| AgentBackendError::RemoteError(format!("A2A submission rejected: {e}")))?;

        response.json::<A2aSubmissionResponse>().await.map_err(|e| {
            AgentBackendError::RemoteError(format!("failed to decode A2A submission: {e}"))
        })
    }

    /// Fetch current task status from the runs endpoint.
    ///
    /// A2A task_send uses `task_id` as the thread_id, so we poll the
    /// latest run for that thread rather than looking up by run_id.
    async fn fetch_task_status(&self, task_id: &str) -> Result<A2aTaskSnapshot, AgentBackendError> {
        let url = format!(
            "{}/v1/threads/{}/runs/latest",
            self.config.base_url.trim_end_matches('/'),
            task_id.trim()
        );

        let response = self
            .build_request(reqwest::Method::GET, &url)
            .send()
            .await
            .map_err(|e| AgentBackendError::RemoteError(format!("failed to query task: {e}")))?;

        let response = response
            .error_for_status()
            .map_err(|e| AgentBackendError::RemoteError(format!("task query rejected: {e}")))?;

        let task_response = response.json::<A2aTaskResponse>().await.map_err(|e| {
            AgentBackendError::RemoteError(format!("failed to decode task status: {e}"))
        })?;

        Ok(map_task_status(&task_response))
    }

    /// Poll until the task reaches a terminal state or timeout.
    async fn poll_to_completion(
        &self,
        task_id: &str,
    ) -> Result<A2aTaskSnapshot, AgentBackendError> {
        let deadline = tokio::time::Instant::now() + self.config.timeout;

        loop {
            let snapshot = self.fetch_task_status(task_id).await?;
            if snapshot.done {
                return Ok(snapshot);
            }

            if tokio::time::Instant::now() >= deadline {
                return Ok(A2aTaskSnapshot {
                    status: RemoteTaskStatus::Timeout,
                    error: Some("polling timeout exceeded".into()),
                    done: true,
                    output_text: snapshot.output_text,
                });
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }
}

#[async_trait]
impl AgentBackend for A2aBackend {
    async fn execute(
        &self,
        agent_id: &str,
        messages: Vec<Message>,
        _event_sink: Arc<dyn EventSink>,
        _parent_run_id: Option<String>,
        _parent_tool_call_id: Option<String>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        // Extract prompt text from user messages
        let prompt = messages
            .iter()
            .filter(|m| m.role == Role::User)
            .flat_map(|m| m.content.iter())
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        if prompt.trim().is_empty() {
            return Err(AgentBackendError::ExecutionFailed(
                "no user message content to send".into(),
            ));
        }

        let submission = self.submit_task(&prompt).await?;

        tracing::info!(
            task_id = %submission.task_id,
            agent_id = %agent_id,
            "a2a_task_submitted"
        );

        let snapshot = self.poll_to_completion(&submission.task_id).await?;

        let (status, steps) = match snapshot.status {
            RemoteTaskStatus::Completed => (DelegateRunStatus::Completed, 1),
            RemoteTaskStatus::Failed => {
                let msg = snapshot
                    .error
                    .unwrap_or_else(|| "remote agent run failed".into());
                (DelegateRunStatus::Failed(msg), 0)
            }
            RemoteTaskStatus::Stopped => (DelegateRunStatus::Cancelled, 0),
            RemoteTaskStatus::Timeout => (DelegateRunStatus::Timeout, 0),
            RemoteTaskStatus::Running => (DelegateRunStatus::Timeout, 0),
        };

        Ok(DelegateRunResult {
            agent_id: agent_id.to_string(),
            status,
            response: snapshot.output_text,
            steps,
            run_id: None,
        })
    }
}

// ---------------------------------------------------------------------------
// A2A protocol types and mapping (moved from remote_a2a.rs)
// ---------------------------------------------------------------------------

/// Submission response from a remote A2A endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct A2aSubmissionResponse {
    task_id: String,
}

/// Task status response from the runs endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct A2aTaskResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub termination_code: Option<String>,
    #[serde(default)]
    pub termination_detail: Option<String>,
    #[serde(default)]
    pub message: Option<Value>,
    #[serde(default)]
    pub history: Vec<Value>,
    #[serde(default)]
    pub artifacts: Vec<Value>,
}

/// Status of a remote A2A task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteTaskStatus {
    Running,
    Completed,
    Failed,
    Stopped,
    Timeout,
}

/// Snapshot of a remote A2A task's state.
#[derive(Debug, Clone)]
pub(crate) struct A2aTaskSnapshot {
    pub status: RemoteTaskStatus,
    pub error: Option<String>,
    pub done: bool,
    pub output_text: Option<String>,
}

pub(crate) fn extract_output_text(response: &A2aTaskResponse) -> Option<String> {
    // Try artifacts first
    for artifact in &response.artifacts {
        if let Some(text) = extract_text_from_value(artifact) {
            return Some(text);
        }
    }
    // Then message
    if let Some(ref message) = response.message
        && let Some(text) = extract_text_from_value(message)
    {
        return Some(text);
    }
    // Then last history entry
    for entry in response.history.iter().rev() {
        if let Some(text) = extract_text_from_value(entry) {
            return Some(text);
        }
    }
    None
}

pub(crate) fn extract_text_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Object(obj) => {
            // Check role filter
            if let Some(role) = obj.get("role").and_then(Value::as_str) {
                let role_lower = role.trim().to_ascii_lowercase();
                if role_lower != "assistant" && role_lower != "agent" {
                    return None;
                }
            }
            if let Some(text) = obj.get("text").and_then(Value::as_str) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            if let Some(content) = obj.get("content") {
                return extract_text_from_value(content);
            }
            if let Some(parts) = obj.get("parts").and_then(Value::as_array) {
                let texts: Vec<String> = parts.iter().filter_map(extract_text_from_value).collect();
                if !texts.is_empty() {
                    return Some(texts.join("\n\n"));
                }
            }
            if let Some(data) = obj.get("data") {
                return match data {
                    Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
                    _ => None,
                };
            }
            None
        }
        Value::Array(items) => {
            let texts: Vec<String> = items.iter().filter_map(extract_text_from_value).collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n\n"))
            }
        }
        _ => None,
    }
}

pub(crate) fn map_task_status(response: &A2aTaskResponse) -> A2aTaskSnapshot {
    let output_text = extract_output_text(response);
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
                    status: RemoteTaskStatus::Stopped,
                    error: response.termination_detail.clone(),
                    done: true,
                    output_text,
                },
                Some("error") => A2aTaskSnapshot {
                    status: RemoteTaskStatus::Failed,
                    error: response
                        .termination_detail
                        .clone()
                        .or_else(|| Some("remote run failed".into())),
                    done: true,
                    output_text,
                },
                _ => A2aTaskSnapshot {
                    status: RemoteTaskStatus::Completed,
                    error: response.termination_detail.clone(),
                    done: true,
                    output_text,
                },
            }
        }
        "failed" | "error" => A2aTaskSnapshot {
            status: RemoteTaskStatus::Failed,
            error: response
                .termination_detail
                .clone()
                .or_else(|| Some("remote run failed".into())),
            done: true,
            output_text,
        },
        "cancelled" | "stopped" => A2aTaskSnapshot {
            status: RemoteTaskStatus::Stopped,
            error: response.termination_detail.clone(),
            done: true,
            output_text,
        },
        _ => A2aTaskSnapshot {
            status: RemoteTaskStatus::Running,
            error: None,
            done: false,
            output_text,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_task_status_completed() {
        let response = A2aTaskResponse {
            status: "completed".into(),
            termination_code: None,
            termination_detail: None,
            message: None,
            history: vec![],
            artifacts: vec![json!({"text": "result"})],
        };
        let snapshot = map_task_status(&response);
        assert_eq!(snapshot.status, RemoteTaskStatus::Completed);
        assert!(snapshot.done);
        assert_eq!(snapshot.output_text.as_deref(), Some("result"));
    }

    #[test]
    fn map_task_status_cancelled() {
        let response = A2aTaskResponse {
            status: "done".into(),
            termination_code: Some("cancelled".into()),
            termination_detail: Some("user cancelled".into()),
            message: None,
            history: vec![],
            artifacts: vec![],
        };
        let snapshot = map_task_status(&response);
        assert_eq!(snapshot.status, RemoteTaskStatus::Stopped);
        assert!(snapshot.done);
        assert_eq!(snapshot.error.as_deref(), Some("user cancelled"));
    }

    #[test]
    fn map_task_status_failed() {
        let response = A2aTaskResponse {
            status: "failed".into(),
            termination_code: None,
            termination_detail: Some("out of memory".into()),
            message: None,
            history: vec![],
            artifacts: vec![],
        };
        let snapshot = map_task_status(&response);
        assert_eq!(snapshot.status, RemoteTaskStatus::Failed);
        assert!(snapshot.done);
    }

    #[test]
    fn map_task_status_running() {
        let response = A2aTaskResponse {
            status: "working".into(),
            termination_code: None,
            termination_detail: None,
            message: None,
            history: vec![],
            artifacts: vec![],
        };
        let snapshot = map_task_status(&response);
        assert_eq!(snapshot.status, RemoteTaskStatus::Running);
        assert!(!snapshot.done);
    }

    #[test]
    fn extract_text_from_artifacts() {
        let response = A2aTaskResponse {
            status: "completed".into(),
            termination_code: None,
            termination_detail: None,
            message: Some(json!({"role": "assistant", "content": "fallback"})),
            history: vec![],
            artifacts: vec![json!({
                "parts": [
                    {"text": "hello"},
                    {"text": "world"}
                ]
            })],
        };
        let text = extract_output_text(&response);
        assert_eq!(text.as_deref(), Some("hello\n\nworld"));
    }

    #[test]
    fn extract_text_from_message_when_no_artifacts() {
        let response = A2aTaskResponse {
            status: "completed".into(),
            termination_code: None,
            termination_detail: None,
            message: Some(json!({"role": "assistant", "text": "reply"})),
            history: vec![],
            artifacts: vec![],
        };
        let text = extract_output_text(&response);
        assert_eq!(text.as_deref(), Some("reply"));
    }

    #[test]
    fn extract_text_from_history_fallback() {
        let response = A2aTaskResponse {
            status: "completed".into(),
            termination_code: None,
            termination_detail: None,
            message: None,
            history: vec![
                json!({"role": "user", "text": "ignored"}),
                json!({"role": "assistant", "text": "last reply"}),
            ],
            artifacts: vec![],
        };
        let text = extract_output_text(&response);
        assert_eq!(text.as_deref(), Some("last reply"));
    }

    #[test]
    fn a2a_config_builder() {
        let config = A2aConfig::new("https://api.example.com")
            .with_bearer_token("tok_123")
            .with_poll_interval(Duration::from_millis(5000))
            .with_timeout(Duration::from_secs(60));

        assert_eq!(config.base_url, "https://api.example.com");
        assert_eq!(config.bearer_token.as_deref(), Some("tok_123"));
        assert_eq!(config.poll_interval, Duration::from_millis(5000));
        assert_eq!(config.timeout, Duration::from_secs(60));
    }

    #[test]
    fn a2a_config_defaults() {
        let config = A2aConfig::new("https://api.example.com");
        assert!(config.bearer_token.is_none());
        assert_eq!(config.poll_interval, Duration::from_millis(2000));
        assert_eq!(config.timeout, Duration::from_secs(300));
    }

    #[test]
    fn map_task_status_done_with_error_termination() {
        let response = A2aTaskResponse {
            status: "done".into(),
            termination_code: Some("error".into()),
            termination_detail: Some("internal error".into()),
            message: None,
            history: vec![],
            artifacts: vec![],
        };
        let snapshot = map_task_status(&response);
        assert_eq!(snapshot.status, RemoteTaskStatus::Failed);
        assert!(snapshot.done);
        assert_eq!(snapshot.error.as_deref(), Some("internal error"));
    }

    #[test]
    fn extract_text_ignores_user_role_in_history() {
        let value = json!({"role": "user", "text": "user input"});
        assert!(extract_text_from_value(&value).is_none());
    }

    #[test]
    fn extract_text_from_data_field() {
        let value = json!({"data": "some data content"});
        let text = extract_text_from_value(&value);
        assert_eq!(text.as_deref(), Some("some data content"));
    }

    #[test]
    fn extract_text_from_nested_content() {
        let value = json!({"role": "assistant", "content": {"text": "nested"}});
        let text = extract_text_from_value(&value);
        assert_eq!(text.as_deref(), Some("nested"));
    }
}
