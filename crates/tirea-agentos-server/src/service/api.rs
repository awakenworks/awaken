use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;
use tirea_agentos::contracts::storage::{MailboxStore, ThreadReader};
use tirea_agentos::runtime::{AgentOs, AgentOsRunError};

use super::mailbox_service::MailboxService;

#[derive(Clone)]
pub struct AppState {
    pub os: Arc<AgentOs>,
    pub read_store: Arc<dyn ThreadReader>,
    pub mailbox_service: Arc<MailboxService>,
}

impl AppState {
    pub fn new(
        os: Arc<AgentOs>,
        read_store: Arc<dyn ThreadReader>,
        mailbox_service: Arc<MailboxService>,
    ) -> Self {
        Self {
            os,
            read_store,
            mailbox_service,
        }
    }

    pub fn mailbox_store(&self) -> &Arc<dyn MailboxStore> {
        self.mailbox_service.mailbox_store()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),

    #[error("thread not found: {0}")]
    ThreadNotFound(String),

    #[error("run not found: {0}")]
    RunNotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, msg) = match &self {
            ApiError::AgentNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            ApiError::ThreadNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            ApiError::RunNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };
        let body = Json(serde_json::json!({ "error": msg }));
        (code, body).into_response()
    }
}

impl From<AgentOsRunError> for ApiError {
    fn from(e: AgentOsRunError) -> Self {
        match e {
            AgentOsRunError::Resolve(
                tirea_agentos::runtime::AgentOsResolveError::AgentNotFound(id),
            ) => ApiError::AgentNotFound(id),
            AgentOsRunError::Resolve(other) => ApiError::BadRequest(other.to_string()),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

pub fn normalize_optional_id(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}
