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

#[derive(serde::Serialize, serde::Deserialize)]
struct BackwardMessageCursor {
    session: String,
    thread: awaken_agent_contract::ThreadId,
    seek: awaken_agent_contract::thread::read::transcript::TranscriptSeek,
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
    use awaken_agent_contract::ThreadId;
    use awaken_agent_contract::thread::read::transcript::TranscriptView;
    use awaken_session_application::MessageListPosition;
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
    let decoded = query
        .page
        .as_deref()
        .map(|token| {
            if token.len() > 4096 {
                return Err(invalid_message_page());
            }
            URL_SAFE_NO_PAD
                .decode(token)
                .map_err(|_| invalid_message_page())
        })
        .transpose()?;
    let backward = decoded
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<BackwardMessageCursor>(bytes).ok());
    let is_backward = query.direction.is_some() || backward.is_some();
    let position = if is_backward {
        match (decoded, backward) {
            (None, _) => MessageListPosition::Latest,
            (Some(_), Some(cursor)) if cursor.session == id && cursor.thread == thread => {
                MessageListPosition::Before(cursor.seek)
            }
            _ => return Err(invalid_message_page()),
        }
    } else if let Some(decoded) = decoded {
        let cursor: ThreadMessageCursor =
            serde_json::from_slice(&decoded).map_err(|_| invalid_message_page())?;
        if cursor.session != id
            || cursor.snapshot.thread_id != thread
            || cursor.snapshot.view != TranscriptView::RawCommitted
            || cursor.start >= cursor.snapshot.end_seq
        {
            return Err(invalid_message_page());
        }
        MessageListPosition::After {
            snapshot: cursor.snapshot,
            start: cursor.start,
        }
    } else {
        MessageListPosition::Beginning
    };

    let page = state
        .roles
        .observation
        .list_messages(&access, &thread, position, limit)
        .await
        .map_err(transcript_error)?;
    let next_page = match page.next {
        Some(MessageListPosition::After { snapshot, start }) => {
            serde_json::to_vec(&ThreadMessageCursor {
                session: id,
                snapshot,
                start,
            })
            .map(Some)
        }
        Some(MessageListPosition::Before(seek)) => serde_json::to_vec(&BackwardMessageCursor {
            session: id,
            thread,
            seek,
        })
        .map(Some),
        _ => Ok(None),
    }
    .map_err(|error| error_response(StateError::Run(RunError::internal(error.to_string()))))?
    .map(|bytes| URL_SAFE_NO_PAD.encode(bytes));
    let data = page
        .messages
        .into_iter()
        .filter_map(|message| present_message(message, is_backward))
        .collect();
    Ok(Json(PageCursor { data, next_page }))
}

fn transcript_error(error: RunError) -> WireErr {
    if error.code == "not_found" {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("not_found_error", error.message)),
        )
    } else {
        error_response(StateError::Run(error))
    }
}

fn present_message(
    message: awaken_agent_contract::Message,
    include_attachments: bool,
) -> Option<ThreadHistoryMessage> {
    use crate::awaken_extensions::ThreadHistoryAttachment;
    use awaken_agent_contract::agent::{
        content::{ContentBlock, DocumentSource, ImageSource},
        message::Role,
    };

    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System | Role::Tool => return None,
    };
    let text = message.text_content();
    let attachments = if include_attachments {
        message
            .content
            .into_iter()
            .filter_map(|block| match block {
                ContentBlock::Image {
                    source: ImageSource::File { file_id },
                } => Some(ThreadHistoryAttachment::Image { file_id }),
                ContentBlock::Document {
                    source: DocumentSource::File { file_id },
                    ..
                } => Some(ThreadHistoryAttachment::Document { file_id }),
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    };
    Some(ThreadHistoryMessage {
        id: message.id.0,
        role,
        text,
        attachments,
    })
}
