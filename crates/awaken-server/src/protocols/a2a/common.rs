use awaken_contract::thread::Thread;
use awaken_protocol_a2a::TaskState;
use axum::extract::Query;
use axum::http::{HeaderMap, Uri};
use serde::de::DeserializeOwned;

use crate::app::ProtocolRoutesState;

use super::error::{A2aError, map_a2a_storage_error};
use super::types::A2A_VERSION;

pub(super) fn parse_a2a_tail(tail: &str) -> Vec<&str> {
    tail.split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

pub(super) fn decode_query<T: DeserializeOwned>(uri: &Uri) -> Result<T, A2aError> {
    Query::<T>::try_from_uri(uri)
        .map(|query| query.0)
        .map_err(|err| A2aError::invalid("query", err.to_string()))
}

pub(super) fn decode_json_body<T: DeserializeOwned>(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<T, A2aError> {
    ensure_json_content_type(headers)?;
    serde_json::from_slice(body)
        .map_err(|err| A2aError::invalid("body", format!("invalid JSON body: {err}")))
}

fn ensure_json_content_type(headers: &HeaderMap) -> Result<(), A2aError> {
    let Some(content_type) = forwarded_header(headers, "content-type") else {
        return Err(A2aError::invalid(
            "contentType",
            "Content-Type must be application/json or application/a2a+json",
        ));
    };

    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    if media_type.eq_ignore_ascii_case("application/json")
        || media_type.eq_ignore_ascii_case("application/a2a+json")
    {
        Ok(())
    } else {
        Err(A2aError::invalid(
            "contentType",
            "Content-Type must be application/json or application/a2a+json",
        ))
    }
}

pub(super) fn parse_page_token(page_token: Option<&str>) -> Result<usize, A2aError> {
    match page_token.map(str::trim).filter(|token| !token.is_empty()) {
        Some(token) => token.parse::<usize>().map_err(|_| {
            A2aError::invalid("pageToken", "pageToken must be an unsigned integer offset")
        }),
        None => Ok(0),
    }
}

pub(super) fn parse_task_state_filter(raw: &str) -> Result<TaskState, A2aError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "task_state_submitted" | "submitted" => Ok(TaskState::Submitted),
        "task_state_working" | "working" => Ok(TaskState::Working),
        "task_state_input_required" | "input_required" | "input-required" => {
            Ok(TaskState::InputRequired)
        }
        "task_state_auth_required" | "auth_required" | "auth-required" => {
            Ok(TaskState::AuthRequired)
        }
        "task_state_completed" | "completed" => Ok(TaskState::Completed),
        "task_state_failed" | "failed" => Ok(TaskState::Failed),
        "task_state_canceled" | "canceled" | "cancelled" => Ok(TaskState::Canceled),
        "task_state_rejected" | "rejected" => Ok(TaskState::Rejected),
        _ => Err(A2aError::invalid(
            "status",
            "status must be a valid TaskState value",
        )),
    }
}

pub(super) fn parse_task_action_segment(raw: &str) -> Result<(String, &str), A2aError> {
    let Some((task_id, action)) = raw.rsplit_once(':') else {
        return Err(A2aError::NotFound(format!(
            "unsupported A2A task action path: {raw}"
        )));
    };

    if task_id.trim().is_empty() {
        return Err(A2aError::invalid(
            "taskId",
            "task action path must include a task id before the action suffix",
        ));
    }

    match action {
        "cancel" | "subscribe" => Ok((task_id.to_string(), action)),
        _ => Err(A2aError::NotFound(format!(
            "unsupported A2A task action path: {raw}"
        ))),
    }
}

pub(super) fn trim_to_option(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn forwarded_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(super) fn ensure_supported_version_from_request(
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<(), A2aError> {
    if let Some(version) = uri
        .query()
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(key, value)| key.eq_ignore_ascii_case("A2A-Version").then_some(value))
        && version != A2A_VERSION
    {
        return Err(A2aError::version_not_supported(version));
    }
    ensure_supported_version(headers)
}

pub(super) fn ensure_supported_version(headers: &HeaderMap) -> Result<(), A2aError> {
    if let Some(version) = forwarded_header(headers, "a2a-version")
        && version != A2A_VERSION
    {
        return Err(A2aError::version_not_supported(version));
    }
    Ok(())
}

pub(super) fn public_agent_id(st: &ProtocolRoutesState) -> Result<String, A2aError> {
    if st.run.resolver.resolve("default").is_ok() {
        return Ok("default".to_string());
    }

    let mut ids = st.run.resolver.agent_ids();
    ids.sort();
    ids.into_iter()
        .find(|id| st.run.resolver.resolve(id).is_ok())
        .ok_or_else(|| A2aError::NotFound("no runnable local agents registered".to_string()))
}

pub(super) fn ensure_runnable_agent(
    st: &ProtocolRoutesState,
    agent_id: &str,
) -> Result<(), A2aError> {
    st.run
        .resolver
        .resolve(agent_id)
        .map(|_| ())
        .map_err(|_| A2aError::NotFound(format!("agent not found: {agent_id}")))
}

pub(super) async fn load_thread_metadata_projection(
    st: &ProtocolRoutesState,
    thread_id: &str,
) -> Result<(bool, Thread), A2aError> {
    let existing = st
        .run
        .store()
        .load_thread(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;
    Ok(materialize_thread_metadata_projection(
        thread_id,
        existing,
        awaken_contract::now_ms(),
    ))
}

pub(super) fn materialize_thread_metadata_projection(
    thread_id: &str,
    existing: Option<Thread>,
    now: u64,
) -> (bool, Thread) {
    let exists = existing.is_some();
    let mut thread = existing.unwrap_or_else(|| Thread::with_id(thread_id));
    thread.touch(now);
    (exists, thread)
}

pub(super) async fn persist_thread_metadata(
    st: &ProtocolRoutesState,
    thread_id: &str,
    exists: bool,
    thread: Thread,
) -> Result<(), A2aError> {
    if exists {
        st.run
            .store()
            .update_thread_metadata(thread_id, thread.metadata)
            .await
            .map_err(map_a2a_storage_error)?;
    } else {
        st.run
            .store()
            .save_thread_validated(&thread)
            .await
            .map_err(map_a2a_storage_error)?;
    }
    Ok(())
}
