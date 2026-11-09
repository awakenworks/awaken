// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: Projects the committed transcript through the existing Session admission and observation ports.
//! Authorized, committed message history for Awaken's Session API.

use axum::Json;
use axum::extract::{Path, Query, rejection::QueryRejection};
use axum::http::StatusCode;

use super::{WireErr, error_response, session_not_found};
use crate::awaken_extensions::{ThreadHistoryMessage, ThreadMessagePageQuery};
use crate::common::scope::RequiredWorkspaceScope;
use crate::state::{ManagedRequestState, StateError, internal_thread_id};
use crate::types::{ErrorResponse, PageCursor};
use awaken_runtime_contract::runs::RunError;

#[derive(serde::Serialize, serde::Deserialize)]
struct ThreadMessageCursor {
    session: String,
    snapshot: awaken_agent_contract::thread::read::transcript::TranscriptSnapshotRef,
    start: u64,
}

fn invalid_message_page() -> WireErr {
    error_response(StateError::Run(RunError::bad_request(
        "invalid transcript page or limit",
    )))
}

/// List messages in a frozen committed Thread transcript after Session admission.
pub async fn list_thread_messages(
    ManagedRequestState(state): ManagedRequestState,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, tid)): Path<(String, String)>,
    query: Result<Query<ThreadMessagePageQuery>, QueryRejection>,
) -> Result<Json<PageCursor<ThreadHistoryMessage>>, WireErr> {
    use awaken_agent_contract::agent::{message::Role, thread::Id as ThreadId};
    use awaken_agent_contract::thread::read::transcript::{TranscriptPageRequest, TranscriptView};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    let session_id = awaken_session_types::SessionId::from(id.as_str());
    let access = crate::state::authorize_session(&state, &workspace, &session_id)
        .await
        .map_err(|error| match error {
            awaken_session_application::SessionAccessError::NotFound => session_not_found(&id),
            error => error_response(StateError::Run(error.into())),
        })?;
    let Query(query) = query.map_err(|_| invalid_message_page())?;
    let thread = ThreadId(internal_thread_id(&id, &tid));
    let limit = std::num::NonZeroUsize::new(query.limit.unwrap_or(50))
        .filter(|limit| limit.get() <= 100)
        .ok_or_else(invalid_message_page)?;
    let mut request = TranscriptPageRequest {
        thread_id: thread,
        view: TranscriptView::RawCommitted,
        snapshot: None,
        start: 0,
        limit,
    };
    if let Some(token) = query.page {
        if token.len() > 4096 {
            return Err(invalid_message_page());
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| invalid_message_page())?;
        let cursor: ThreadMessageCursor =
            serde_json::from_slice(&decoded).map_err(|_| invalid_message_page())?;
        if cursor.session != id
            || cursor.snapshot.thread_id != request.thread_id
            || cursor.snapshot.view != request.view
            || cursor.start >= cursor.snapshot.end_seq
        {
            return Err(invalid_message_page());
        }
        request.snapshot = Some(cursor.snapshot);
        request.start = cursor.start;
    }
    let page = state
        .roles
        .observation
        .transcript_page(&access, &request)
        .await
        .map_err(|error| {
            if error.code == "not_found" {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse::new("not_found_error", error.message)),
                )
            } else {
                error_response(StateError::Run(error))
            }
        })?;
    let next_page = page
        .next_start()
        .map(|start| {
            let cursor = ThreadMessageCursor {
                session: id,
                snapshot: page.snapshot,
                start,
            };
            serde_json::to_vec(&cursor).map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
        })
        .transpose()
        .map_err(|error| error_response(StateError::Run(RunError::internal(error.to_string()))))?;
    let data = page
        .messages
        .into_iter()
        .filter_map(|message| {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System | Role::Tool => return None,
            };
            let text = message.text_content();
            Some(ThreadHistoryMessage {
                id: message.id.0,
                role,
                text,
            })
        })
        .collect();
    Ok(Json(PageCursor { data, next_page }))
}
