//! Scheduled agent delegation backend -- dispatches to ARD Server's Scheduler.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::message::{Message, Role};
use awaken_contract::registry_spec::{RemoteEndpoint, SchedulingPolicy};
use serde::{Deserialize, Serialize};

use super::backend::{
    AgentBackend, AgentBackendError, AgentBackendFactory, AgentBackendFactoryError,
    DelegateRunResult, DelegateRunStatus,
};

const SCHEDULED_BACKEND: &str = "scheduled";
const ADAPTER_TYPE_OPTION_KEY: &str = "adapter_type";
const SCHEDULING_OPTION_KEY: &str = "scheduling";

/// Configuration for a scheduler-dispatched agent endpoint.
#[derive(Debug, Clone)]
pub struct ScheduledConfig {
    /// Base URL of the scheduler (e.g. `http://127.0.0.1:7878`).
    pub scheduler_url: String,
    /// Adapter type on the Worker, e.g. "claude", "codex", "openclaw".
    pub adapter_type: String,
    /// Scheduling policy to forward to the scheduler.
    pub scheduling: SchedulingPolicy,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ScheduledEndpointConfigError {
    #[error("remote endpoint backend must be `scheduled`, got `{0}`")]
    UnsupportedBackend(String),
    #[error("remote endpoint base_url must not be empty")]
    EmptyBaseUrl,
    #[error("scheduled backend requires `adapter_type` in options")]
    MissingAdapterType,
    #[error("scheduled backend requires a valid `scheduling` option: {0}")]
    InvalidSchedulingPolicy(String),
}

impl ScheduledConfig {
    pub(crate) fn try_from_remote_endpoint(
        endpoint: &RemoteEndpoint,
    ) -> Result<Self, ScheduledEndpointConfigError> {
        if endpoint.backend != SCHEDULED_BACKEND {
            return Err(ScheduledEndpointConfigError::UnsupportedBackend(
                endpoint.backend.clone(),
            ));
        }

        if endpoint.base_url.trim().is_empty() {
            return Err(ScheduledEndpointConfigError::EmptyBaseUrl);
        }

        let adapter_type = endpoint
            .options
            .get(ADAPTER_TYPE_OPTION_KEY)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or(ScheduledEndpointConfigError::MissingAdapterType)?;

        let scheduling = endpoint
            .options
            .get(SCHEDULING_OPTION_KEY)
            .map(|value| {
                serde_json::from_value::<SchedulingPolicy>(value.clone()).map_err(|error| {
                    ScheduledEndpointConfigError::InvalidSchedulingPolicy(error.to_string())
                })
            })
            .transpose()?
            .unwrap_or_default();

        Ok(Self {
            scheduler_url: endpoint.base_url.clone(),
            adapter_type: adapter_type.to_string(),
            scheduling,
        })
    }
}

/// Backend that dispatches agent execution to the ARD Server's Scheduler.
pub struct ScheduledBackend {
    config: ScheduledConfig,
    client: reqwest::Client,
}

/// Factory for the built-in scheduled remote backend.
pub struct ScheduledBackendFactory;

impl AgentBackendFactory for ScheduledBackendFactory {
    fn backend(&self) -> &str {
        SCHEDULED_BACKEND
    }

    fn build(
        &self,
        endpoint: &RemoteEndpoint,
    ) -> Result<Arc<dyn AgentBackend>, AgentBackendFactoryError> {
        let config = ScheduledConfig::try_from_remote_endpoint(endpoint)
            .map_err(|error| AgentBackendFactoryError::InvalidConfig(error.to_string()))?;
        Ok(Arc::new(ScheduledBackend::new(config)))
    }
}

impl ScheduledBackend {
    /// Create a new scheduled backend with the given configuration.
    pub fn new(config: ScheduledConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }
}

/// Request body for the `/internal/dispatch` endpoint.
#[derive(Debug, Serialize)]
struct DispatchRequest {
    agent_id: String,
    adapter_type: String,
    scheduling: SchedulingPolicy,
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_run_id: Option<String>,
}

/// Response body from the `/internal/dispatch` endpoint.
#[derive(Debug, Deserialize)]
struct DispatchResponse {
    status: String,
    #[serde(default)]
    response: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    steps: usize,
    #[serde(default)]
    run_id: Option<String>,
}

#[async_trait]
impl AgentBackend for ScheduledBackend {
    async fn execute(
        &self,
        agent_id: &str,
        messages: Vec<Message>,
        _event_sink: Arc<dyn EventSink>,
        parent_run_id: Option<String>,
        _parent_thread_id: Option<String>,
        _parent_tool_call_id: Option<String>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        let prompt = messages
            .iter()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| message.content.iter())
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

        let url = format!(
            "{}/internal/dispatch",
            self.config.scheduler_url.trim_end_matches('/')
        );

        let request = DispatchRequest {
            agent_id: agent_id.to_string(),
            adapter_type: self.config.adapter_type.clone(),
            scheduling: self.config.scheduling.clone(),
            prompt,
            parent_run_id,
        };

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                AgentBackendError::RemoteError(format!("failed to dispatch to scheduler: {e}"))
            })?;

        let response = response.error_for_status().map_err(|e| {
            AgentBackendError::RemoteError(format!("scheduler dispatch rejected: {e}"))
        })?;

        let dispatch: DispatchResponse = response.json().await.map_err(|e| {
            AgentBackendError::RemoteError(format!("failed to decode scheduler response: {e}"))
        })?;

        let status = match dispatch.status.as_str() {
            "completed" => DelegateRunStatus::Completed,
            "cancelled" => DelegateRunStatus::Cancelled,
            "timeout" => DelegateRunStatus::Timeout,
            "failed" => DelegateRunStatus::Failed(
                dispatch
                    .error
                    .unwrap_or_else(|| "scheduler dispatch failed".into()),
            ),
            other => DelegateRunStatus::Failed(format!("unknown dispatch status: {other}")),
        };

        Ok(DelegateRunResult {
            agent_id: agent_id.to_string(),
            status,
            response: dispatch.response,
            steps: dispatch.steps,
            run_id: dispatch.run_id,
            inbox: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use serde_json::json;

    #[test]
    fn config_from_endpoint_extracts_adapter_type() {
        let mut options = BTreeMap::new();
        options.insert(ADAPTER_TYPE_OPTION_KEY.into(), json!("claude"));
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "http://127.0.0.1:7878".into(),
            options,
            ..Default::default()
        };

        let config = ScheduledConfig::try_from_remote_endpoint(&endpoint).unwrap();
        assert_eq!(config.scheduler_url, "http://127.0.0.1:7878");
        assert_eq!(config.adapter_type, "claude");
        assert!(matches!(
            config.scheduling,
            SchedulingPolicy::SessionAffinity
        ));
    }

    #[test]
    fn config_from_endpoint_requires_adapter_type() {
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "http://127.0.0.1:7878".into(),
            ..Default::default()
        };

        let err = ScheduledConfig::try_from_remote_endpoint(&endpoint).unwrap_err();
        assert!(err.to_string().contains("adapter_type"));
    }

    #[test]
    fn factory_backend_name_is_scheduled() {
        let factory = ScheduledBackendFactory;
        assert_eq!(factory.backend(), "scheduled");
    }

    #[test]
    fn config_from_endpoint_rejects_wrong_backend() {
        let endpoint = RemoteEndpoint {
            backend: "a2a".into(),
            base_url: "http://127.0.0.1:7878".into(),
            ..Default::default()
        };

        let err = ScheduledConfig::try_from_remote_endpoint(&endpoint).unwrap_err();
        assert!(err.to_string().contains("scheduled"));
    }

    #[test]
    fn config_from_endpoint_rejects_empty_base_url() {
        let mut options = BTreeMap::new();
        options.insert(ADAPTER_TYPE_OPTION_KEY.into(), json!("claude"));
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "".into(),
            options,
            ..Default::default()
        };

        let err = ScheduledConfig::try_from_remote_endpoint(&endpoint).unwrap_err();
        assert!(err.to_string().contains("base_url"));
    }

    #[test]
    fn factory_builds_backend_for_scheduled_endpoint() {
        let mut options = BTreeMap::new();
        options.insert(ADAPTER_TYPE_OPTION_KEY.into(), json!("codex"));
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "http://127.0.0.1:7878".into(),
            options,
            ..Default::default()
        };

        let backend = ScheduledBackendFactory.build(&endpoint).unwrap();
        let _backend: Arc<dyn AgentBackend> = backend;
    }

    #[test]
    fn config_from_endpoint_extracts_scheduling_policy() {
        let mut options = BTreeMap::new();
        options.insert(ADAPTER_TYPE_OPTION_KEY.into(), json!("codex"));
        options.insert(
            SCHEDULING_OPTION_KEY.into(),
            serde_json::to_value(SchedulingPolicy::Pinned {
                node_id: "node-7".into(),
            })
            .unwrap(),
        );
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "http://127.0.0.1:7878".into(),
            options,
            ..Default::default()
        };

        let config = ScheduledConfig::try_from_remote_endpoint(&endpoint).unwrap();
        assert_eq!(config.adapter_type, "codex");
        assert!(matches!(
            config.scheduling,
            SchedulingPolicy::Pinned { ref node_id } if node_id == "node-7"
        ));
    }

    #[test]
    fn config_from_endpoint_rejects_invalid_scheduling_policy() {
        let mut options = BTreeMap::new();
        options.insert(ADAPTER_TYPE_OPTION_KEY.into(), json!("codex"));
        options.insert(
            SCHEDULING_OPTION_KEY.into(),
            json!({ "pinned": { "node_id": 7 } }),
        );
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "http://127.0.0.1:7878".into(),
            options,
            ..Default::default()
        };

        let err = ScheduledConfig::try_from_remote_endpoint(&endpoint).unwrap_err();
        assert!(err.to_string().contains("scheduling"));
    }

    #[test]
    fn factory_rejects_missing_adapter_type() {
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "http://127.0.0.1:7878".into(),
            ..Default::default()
        };

        let err = ScheduledBackendFactory
            .build(&endpoint)
            .err()
            .expect("should fail");
        assert!(err.to_string().contains("adapter_type"));
    }

    #[test]
    fn config_with_bearer_token_extracts_correctly() {
        let mut options = BTreeMap::new();
        options.insert(ADAPTER_TYPE_OPTION_KEY.into(), json!("claude"));
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "http://127.0.0.1:7878".into(),
            auth: Some(awaken_contract::registry_spec::RemoteAuth::bearer(
                "secret-token",
            )),
            options,
            ..Default::default()
        };

        let config = ScheduledConfig::try_from_remote_endpoint(&endpoint).unwrap();
        assert_eq!(config.scheduler_url, "http://127.0.0.1:7878");
        assert_eq!(config.adapter_type, "claude");
        // Auth is not part of ScheduledConfig but the endpoint parses without error
        assert_eq!(
            endpoint.auth.as_ref().unwrap().param_str("token"),
            Some("secret-token")
        );
    }

    #[test]
    fn config_with_empty_base_url_still_rejected() {
        let mut options = BTreeMap::new();
        options.insert(ADAPTER_TYPE_OPTION_KEY.into(), json!("claude"));
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "".into(),
            options,
            ..Default::default()
        };

        let err = ScheduledConfig::try_from_remote_endpoint(&endpoint).unwrap_err();
        assert!(
            err.to_string().contains("base_url"),
            "expected base_url error, got: {err}"
        );
    }

    #[test]
    fn config_with_whitespace_only_base_url_rejected() {
        let mut options = BTreeMap::new();
        options.insert(ADAPTER_TYPE_OPTION_KEY.into(), json!("claude"));
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "   ".into(),
            options,
            ..Default::default()
        };

        let err = ScheduledConfig::try_from_remote_endpoint(&endpoint).unwrap_err();
        assert!(err.to_string().contains("base_url"));
    }

    #[test]
    fn factory_rejects_endpoint_with_wrong_backend_name() {
        let mut options = BTreeMap::new();
        options.insert(ADAPTER_TYPE_OPTION_KEY.into(), json!("claude"));
        let endpoint = RemoteEndpoint {
            backend: "a2a".into(),
            base_url: "http://127.0.0.1:7878".into(),
            options,
            ..Default::default()
        };

        let err = ScheduledBackendFactory
            .build(&endpoint)
            .err()
            .expect("should fail for non-scheduled backend");
        assert!(
            err.to_string().contains("scheduled"),
            "error should mention 'scheduled', got: {err}"
        );
    }

    #[test]
    fn config_with_empty_adapter_type_in_options_rejected() {
        let mut options = BTreeMap::new();
        options.insert(ADAPTER_TYPE_OPTION_KEY.into(), json!(""));
        let endpoint = RemoteEndpoint {
            backend: "scheduled".into(),
            base_url: "http://127.0.0.1:7878".into(),
            options,
            ..Default::default()
        };

        let err = ScheduledConfig::try_from_remote_endpoint(&endpoint).unwrap_err();
        assert!(
            err.to_string().contains("adapter_type"),
            "expected adapter_type error, got: {err}"
        );
    }

    #[test]
    fn dispatch_request_serializes_scheduling_policy() {
        let request = DispatchRequest {
            agent_id: "worker".into(),
            adapter_type: "codex".into(),
            scheduling: SchedulingPolicy::LeastLoaded,
            prompt: "hello".into(),
            parent_run_id: Some("run-1".into()),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["agent_id"], "worker");
        assert_eq!(json["adapter_type"], "codex");
        assert_eq!(json["scheduling"], "least_loaded");
        assert_eq!(json["prompt"], "hello");
        assert_eq!(json["parent_run_id"], "run-1");
    }
}
