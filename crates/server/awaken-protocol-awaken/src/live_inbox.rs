//! Awaken's edit-in-flight queue protocol.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_contract::{
    LiveInboxApplication, LiveInboxApplicationError, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot,
};
use awaken_tenancy::WorkspaceScope;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Extension, Json, Router};

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

#[derive(Debug, serde::Serialize)]
struct ExtensionErrorBody {
    r#type: &'static str,
    error: ExtensionError,
}

#[derive(Debug, serde::Serialize)]
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

fn required_workspace_scope(
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<String, ExtensionRejection> {
    scope
        .as_ref()
        .and_then(|Extension(scope)| scope.non_empty())
        .map(str::to_owned)
        // Missing/empty and foreign scopes are deliberately indistinguishable
        // from an unknown Session at this extension boundary.
        .ok_or_else(|| extension_error(LiveInboxApplicationError::NotFound))
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
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> Result<Json<WireLiveInboxSnapshot>, ExtensionRejection> {
    let workspace_id = required_workspace_scope(scope)?;
    state
        .snapshot(&workspace_id, &id)
        .await
        .map(|snapshot| Json(project_snapshot(snapshot)))
        .map_err(extension_error)
}

async fn live_inbox_queue(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
    Json(body): Json<LiveInboxMessageBody>,
) -> Result<Json<LiveInboxQueuedResponse>, ExtensionRejection> {
    let workspace_id = required_workspace_scope(scope)?;
    state
        .queue(&workspace_id, &id, body.content)
        .await
        .map(|id| Json(LiveInboxQueuedResponse { id }))
        .map_err(extension_error)
}

async fn live_inbox_remove(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((id, msg)): Path<(String, u64)>,
) -> Result<StatusCode, ExtensionRejection> {
    let workspace_id = required_workspace_scope(scope)?;
    state
        .remove(&workspace_id, &id, msg)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(extension_error)
}

async fn live_inbox_replace(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((id, msg)): Path<(String, u64)>,
    Json(body): Json<LiveInboxMessageBody>,
) -> Result<StatusCode, ExtensionRejection> {
    let workspace_id = required_workspace_scope(scope)?;
    state
        .replace(&workspace_id, &id, msg, body.content)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(extension_error)
}

async fn live_inbox_reorder(
    State(state): State<Arc<dyn LiveInboxApplication>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
    Json(body): Json<LiveInboxOrderBody>,
) -> Result<StatusCode, ExtensionRejection> {
    let workspace_id = required_workspace_scope(scope)?;
    state
        .reorder(&workspace_id, &id, body.order)
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
        // infrastructure unavailable -> E4 500. These terminal outcomes must not
        // inherit Anthropic routing.
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
            extension_error(LiveInboxApplicationError::Unavailable("down".into())).0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn workspace_scope_decision_table_fails_closed() {
        // FMECA C/E graph and decision table:
        // C1 edge scope missing; C2 edge scope empty; C3 edge scope valid.
        // E1 no application call and 404; E2 exact scope reaches the application.
        // R1 !C1,!C2,C3 -> E2; R2 C1 -> E1; R3 C2 -> E1. This prevents an
        // unscoped route mount from exposing another Workspace's live attempt.
        assert_eq!(
            required_workspace_scope(None).unwrap_err().0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            required_workspace_scope(Some(Extension(WorkspaceScope(" ".into()))))
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            required_workspace_scope(Some(Extension(WorkspaceScope("workspace-a".into()))))
                .unwrap(),
            "workspace-a"
        );
    }
}
