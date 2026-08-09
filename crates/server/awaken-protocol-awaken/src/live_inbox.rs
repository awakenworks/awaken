//! Awaken's edit-in-flight queue protocol.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_contract::{
    LiveInboxApplication, LiveInboxApplicationError, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};

#[derive(serde::Serialize)]
struct WireLiveInboxEntry {
    id: u64,
    content: Vec<ContentBlock>,
}

#[derive(serde::Serialize)]
struct WireLiveInboxSnapshot {
    active: bool,
    version: u64,
    messages: Vec<WireLiveInboxEntry>,
}

#[derive(serde::Serialize)]
struct ExtensionErrorBody {
    r#type: &'static str,
    error: ExtensionError,
}

#[derive(serde::Serialize)]
struct ExtensionError {
    r#type: &'static str,
    message: String,
}

type ExtensionRejection = (StatusCode, Json<ExtensionErrorBody>);

fn extension_error(error: LiveInboxApplicationError) -> ExtensionRejection {
    let (status, kind) = match &error {
        LiveInboxApplicationError::NotFound
        | LiveInboxApplicationError::Edit(LiveInboxError::UnknownMessage) => {
            (StatusCode::NOT_FOUND, "not_found_error")
        }
        LiveInboxApplicationError::Edit(LiveInboxError::StaleOrder) => {
            (StatusCode::CONFLICT, "invalid_request_error")
        }
        LiveInboxApplicationError::Edit(LiveInboxError::Inactive) => {
            (StatusCode::GONE, "invalid_request_error")
        }
        LiveInboxApplicationError::Unavailable(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "api_error")
        }
    };
    (
        status,
        Json(ExtensionErrorBody {
            r#type: "error",
            error: ExtensionError {
                r#type: kind,
                message: error.to_string(),
            },
        }),
    )
}

fn project_snapshot(snapshot: LiveInboxSnapshot) -> WireLiveInboxSnapshot {
    WireLiveInboxSnapshot {
        active: snapshot.active,
        version: snapshot.version,
        messages: snapshot
            .messages
            .into_iter()
            .map(|LiveInboxEntry { id, content }| WireLiveInboxEntry { id, content })
            .collect(),
    }
}

/// Mount the explicitly namespaced Awaken live-inbox routes.
pub fn live_inbox_router(state: Arc<dyn LiveInboxApplication>) -> Router {
    Router::new()
        .route(
            "/v1/awaken/sessions/{id}/live-inbox",
            get(live_inbox_snapshot).post(live_inbox_queue),
        )
        .route(
            "/v1/awaken/sessions/{id}/live-inbox/order",
            put(live_inbox_reorder),
        )
        .route(
            "/v1/awaken/sessions/{id}/live-inbox/{msg}",
            put(live_inbox_replace).delete(live_inbox_remove),
        )
        .with_state(state)
}

#[derive(serde::Deserialize)]
struct LiveInboxMessageBody {
    content: Vec<ContentBlock>,
}

#[derive(serde::Deserialize)]
struct LiveInboxOrderBody {
    order: Vec<u64>,
}

#[derive(serde::Serialize)]
struct LiveInboxQueuedResponse {
    id: u64,
}

async fn live_inbox_snapshot(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    Path(id): Path<String>,
) -> Result<Json<WireLiveInboxSnapshot>, ExtensionRejection> {
    state
        .snapshot(&id)
        .await
        .map(|snapshot| Json(project_snapshot(snapshot)))
        .map_err(extension_error)
}

async fn live_inbox_queue(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    Path(id): Path<String>,
    Json(body): Json<LiveInboxMessageBody>,
) -> Result<Json<LiveInboxQueuedResponse>, ExtensionRejection> {
    state
        .queue(&id, body.content)
        .await
        .map(|id| Json(LiveInboxQueuedResponse { id }))
        .map_err(extension_error)
}

async fn live_inbox_remove(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    Path((id, msg)): Path<(String, u64)>,
) -> Result<StatusCode, ExtensionRejection> {
    state
        .remove(&id, msg)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(extension_error)
}

async fn live_inbox_replace(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    Path((id, msg)): Path<(String, u64)>,
    Json(body): Json<LiveInboxMessageBody>,
) -> Result<StatusCode, ExtensionRejection> {
    state
        .replace(&id, msg, body.content)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(extension_error)
}

async fn live_inbox_reorder(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    Path(id): Path<String>,
    Json(body): Json<LiveInboxOrderBody>,
) -> Result<StatusCode, ExtensionRejection> {
    state
        .reorder(&id, body.order)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(extension_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_inbox_error_decision_table_is_explicit() {
        // Cause/effect decision table: C1 missing Session or message -> E1 404;
        // C2 stale order -> E2 409; C3 inactive attempt -> E3 410. These are the
        // three terminal edit outcomes; they must not inherit Anthropic routing.
        assert_eq!(
            extension_error(LiveInboxApplicationError::NotFound).0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            extension_error(LiveInboxError::StaleOrder.into()).0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            extension_error(LiveInboxError::Inactive.into()).0,
            StatusCode::GONE
        );
    }
}
