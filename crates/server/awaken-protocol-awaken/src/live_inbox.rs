// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This source implements the repository-controlled session product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Awaken's edit-in-flight queue protocol.

use std::sync::Arc;

use crate::common::{ExtensionRejection, RequiredWorkspaceScope, extension_rejection};
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_application_contract::{
    LiveInboxApplication, LiveInboxApplicationError, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot,
};
#[cfg(test)]
use awaken_tenancy::WorkspaceScope;
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
        LiveInboxApplicationError::Edit(LiveInboxError::Full) => {
            (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error")
        }
        LiveInboxApplicationError::AuthorityUnavailable { .. }
        | LiveInboxApplicationError::Edit(LiveInboxError::Unavailable) => {
            (StatusCode::SERVICE_UNAVAILABLE, "api_error")
        }
        LiveInboxApplicationError::InvalidSessionState { .. } => {
            (StatusCode::INTERNAL_SERVER_ERROR, "api_error")
        }
    };
    extension_rejection(status, kind, error.to_string())
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
#[serde(deny_unknown_fields)]
struct LiveInboxMessageBody {
    content: Vec<ContentBlock>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveInboxOrderBody {
    order: Vec<u64>,
}

#[derive(serde::Serialize)]
struct LiveInboxQueuedResponse {
    id: u64,
}

async fn live_inbox_snapshot(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> Result<Json<WireLiveInboxSnapshot>, ExtensionRejection> {
    state
        .snapshot(&workspace_id, &awaken_session_types::SessionId::from(id))
        .await
        .map(|snapshot| Json(project_snapshot(snapshot)))
        .map_err(extension_error)
}

async fn live_inbox_queue(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(id): Path<String>,
    Json(body): Json<LiveInboxMessageBody>,
) -> Result<Json<LiveInboxQueuedResponse>, ExtensionRejection> {
    state
        .queue(
            &workspace_id,
            &awaken_session_types::SessionId::from(id),
            body.content,
        )
        .await
        .map(|id| Json(LiveInboxQueuedResponse { id }))
        .map_err(extension_error)
}

async fn live_inbox_remove(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path((id, msg)): Path<(String, u64)>,
) -> Result<StatusCode, ExtensionRejection> {
    state
        .remove(
            &workspace_id,
            &awaken_session_types::SessionId::from(id),
            msg,
        )
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(extension_error)
}

async fn live_inbox_replace(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path((id, msg)): Path<(String, u64)>,
    Json(body): Json<LiveInboxMessageBody>,
) -> Result<StatusCode, ExtensionRejection> {
    state
        .replace(
            &workspace_id,
            &awaken_session_types::SessionId::from(id),
            msg,
            body.content,
        )
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(extension_error)
}

async fn live_inbox_reorder(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(id): Path<String>,
    Json(body): Json<LiveInboxOrderBody>,
) -> Result<StatusCode, ExtensionRejection> {
    state
        .reorder(
            &workspace_id,
            &awaken_session_types::SessionId::from(id),
            body.order,
        )
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(extension_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_inbox_error_decision_table_is_explicit() {
        // Cause/effect decision table: C1 missing/foreign Session or message ->
        // E1 404; C2 stale order -> E2 409; C3 inactive attempt -> E3 410; C4
        // infrastructure unavailable, including an unreadable live-inbox owner
        // registry -> E4 503; C5 corrupt authority state -> E5 500; C6 a full
        // inbox (backpressure) -> E6 429.
        // These outcomes must not inherit Anthropic routing.
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
        assert_eq!(
            extension_error(LiveInboxApplicationError::AuthorityUnavailable {
                detail: "down".into(),
            })
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            extension_error(LiveInboxApplicationError::InvalidSessionState {
                detail: "down".into()
            })
            .0,
            StatusCode::INTERNAL_SERVER_ERROR,
            "C5 same detail, permanent fault"
        );
        assert_eq!(
            extension_error(LiveInboxError::Unavailable.into()).0,
            StatusCode::SERVICE_UNAVAILABLE,
            "C4 unreadable owner registry"
        );
        assert_eq!(
            extension_error(LiveInboxError::Full.into()).0,
            StatusCode::TOO_MANY_REQUESTS,
            "C6 backpressure"
        );
    }

    #[tokio::test]
    async fn workspace_scope_decision_table_fails_closed() {
        // FMECA C/E graph and decision table:
        // C1 edge scope missing; C2 edge scope empty; C3 edge scope valid.
        // E1 no application call and 404; E2 exact scope reaches the application.
        // R1 !C1,!C2,C3 -> E2; R2 C1 -> E1; R3 C2 -> E1. This prevents an
        // unscoped route mount from exposing another Workspace's live attempt.
        assert_eq!(
            extract_scope(None).await.unwrap_err(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            extract_scope(Some(" ")).await.unwrap_err(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            extract_scope(Some("workspace-a")).await.unwrap(),
            "workspace-a"
        );
    }

    /// Exercise the extractor the way axum does, so absence is tested where it
    /// is actually decided rather than through a deleted helper function.
    async fn extract_scope(stamp: Option<&str>) -> Result<String, StatusCode> {
        use axum::extract::FromRequestParts;
        let mut parts = axum::http::Request::new(()).into_parts().0;
        if let Some(stamp) = stamp {
            parts.extensions.insert(WorkspaceScope(stamp.to_string()));
        }
        RequiredWorkspaceScope::from_request_parts(&mut parts, &())
            .await
            .map(|RequiredWorkspaceScope(scope)| scope)
            .map_err(|rejection| {
                use axum::response::IntoResponse;
                rejection.into_response().status()
            })
    }
}
