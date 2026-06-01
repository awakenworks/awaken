use std::collections::BTreeSet;

use awaken_protocol_a2a::{ListTasksResponse, MessageRole, Task, TaskState, TaskStatus};
use awaken_server_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_server_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_server_contract::contract::storage::{RunQuery, RunRecord, WaitingReason};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde_json::json;

use crate::app::ProtocolRoutesState;

use super::common::{
    ensure_runnable_agent, ensure_supported_version, load_thread_metadata_projection,
    parse_page_token, parse_task_state_filter, persist_thread_metadata,
};
use super::conversion::{awaken_message_to_a2a_message, message_to_artifacts};
use super::error::A2aError;

use super::types::{
    BLOCKING_POLL_INTERVAL, BLOCKING_WAIT_TIMEOUT, DEFAULT_PAGE_SIZE, GetTaskQuery, ListTasksQuery,
    MAX_PAGE_SIZE, ResolvedTask, StoredTaskBinding, StoredTaskBindings, TASK_BINDINGS_METADATA_KEY,
    TaskSnapshot, TaskSource,
};

pub(super) async fn a2a_list_tasks_default(
    State(st): State<ProtocolRoutesState>,
    headers: HeaderMap,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<ListTasksResponse>, A2aError> {
    list_tasks(st, headers, None, query).await
}

pub(super) async fn a2a_list_tasks_tenant(
    State(st): State<ProtocolRoutesState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<ListTasksResponse>, A2aError> {
    list_tasks(st, headers, Some(tenant), query).await
}

pub(super) async fn a2a_get_task_default(
    State(st): State<ProtocolRoutesState>,
    Path(task_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<GetTaskQuery>,
) -> Result<Json<Task>, A2aError> {
    get_task(st, headers, None, task_id, query).await
}

pub(super) async fn a2a_get_task_tenant(
    State(st): State<ProtocolRoutesState>,
    Path((tenant, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<GetTaskQuery>,
) -> Result<Json<Task>, A2aError> {
    get_task(st, headers, Some(tenant), task_id, query).await
}

pub(super) async fn cancel_task(
    st: ProtocolRoutesState,
    headers: HeaderMap,
    tenant: Option<String>,
    task_id: String,
) -> Result<Json<Task>, A2aError> {
    ensure_supported_version(&headers)?;
    if let Some(ref tenant) = tenant {
        ensure_runnable_agent(&st, tenant)?;
    }

    let existing = load_task_snapshot(&st, &task_id, tenant.as_deref(), usize::MAX, true)
        .await?
        .ok_or_else(|| A2aError::task_not_found(task_id.clone()))?;

    if existing.task.status.state.is_terminal() {
        return Err(A2aError::task_not_cancelable(
            task_id,
            existing.task.status.state,
        ));
    }

    let mailbox = st.run.mailbox();
    let queued_dispatches = mailbox
        .list_dispatches(
            &st.run.scoped_id(&existing.task.id),
            Some(&[RunDispatchStatus::Queued]),
            100,
            0,
        )
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;

    let mut cancelled = false;
    for dispatch in queued_dispatches {
        cancelled |= mailbox
            .cancel(&dispatch.dispatch_id())
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
    }
    cancelled |= mailbox
        .cancel(&st.run.scoped_id(&existing.task.id))
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?;

    if !cancelled {
        return Err(A2aError::task_not_cancelable(
            existing.task.id,
            existing.task.status.state,
        ));
    }

    let task = load_task_snapshot(&st, &existing.task.id, tenant.as_deref(), usize::MAX, true)
        .await?
        .map(|snapshot| snapshot.task)
        .unwrap_or_else(|| {
            canceled_task(
                &existing.task.id,
                &existing.task.context_id,
                existing.current_agent_id.as_deref(),
            )
        });

    Ok(Json(task))
}

async fn list_tasks(
    st: ProtocolRoutesState,
    headers: HeaderMap,
    tenant: Option<String>,
    query: ListTasksQuery,
) -> Result<Json<ListTasksResponse>, A2aError> {
    ensure_supported_version(&headers)?;
    if let Some(ref tenant) = tenant {
        ensure_runnable_agent(&st, tenant)?;
    }

    let page_size = query
        .page_size
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    let offset = parse_page_token(query.page_token.as_deref())?;
    let history_length = query.history_length.unwrap_or(0);
    let status_filter = query
        .status
        .as_deref()
        .map(parse_task_state_filter)
        .transpose()?;

    let mut snapshots = Vec::new();
    for task_id in collect_task_ids(&st).await? {
        let Some(snapshot) =
            load_task_snapshot(&st, &task_id, tenant.as_deref(), history_length, false).await?
        else {
            continue;
        };

        if let Some(ref context_id) = query.context_id
            && snapshot.task.context_id != *context_id
        {
            continue;
        }
        if let Some(expected) = status_filter
            && snapshot.task.status.state != expected
        {
            continue;
        }
        snapshots.push(snapshot);
    }

    snapshots.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.task.id.cmp(&right.task.id))
    });

    let total_size = snapshots.len();
    let tasks = snapshots
        .into_iter()
        .skip(offset)
        .take(page_size)
        .map(|snapshot| snapshot.task)
        .collect::<Vec<_>>();
    let next_offset = offset + tasks.len();
    let next_page_token = if next_offset < total_size {
        next_offset.to_string()
    } else {
        String::new()
    };

    Ok(Json(ListTasksResponse {
        tasks,
        total_size,
        page_size,
        next_page_token,
    }))
}

async fn get_task(
    st: ProtocolRoutesState,
    headers: HeaderMap,
    tenant: Option<String>,
    task_id: String,
    query: GetTaskQuery,
) -> Result<Json<Task>, A2aError> {
    ensure_supported_version(&headers)?;
    if let Some(ref tenant) = tenant {
        ensure_runnable_agent(&st, tenant)?;
    }
    let history_length = query.history_length.unwrap_or(usize::MAX);
    let snapshot = load_task_snapshot(&st, &task_id, tenant.as_deref(), history_length, true)
        .await?
        .ok_or_else(|| A2aError::task_not_found(task_id.clone()))?;
    Ok(Json(snapshot.task))
}

pub(super) async fn resolve_task(
    st: &ProtocolRoutesState,
    task_id: &str,
) -> Result<Option<ResolvedTask>, A2aError> {
    if let Some(run) = st
        .run
        .store()
        .load_run(task_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
    {
        let dispatch = st
            .run
            .mailbox()
            .load_dispatch(&st.run.scoped_id(task_id))
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?
            .map(|dispatch| st.run.unscope_dispatch(dispatch));
        return Ok(Some(ResolvedTask {
            thread_id: run.thread_id.clone(),
            run: Some(run),
            dispatch,
        }));
    }

    let Some(dispatch) = st
        .run
        .mailbox()
        .load_dispatch(&st.run.scoped_id(task_id))
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .map(|dispatch| st.run.unscope_dispatch(dispatch))
    else {
        return Ok(None);
    };
    Ok(Some(ResolvedTask {
        thread_id: dispatch.thread_id().clone(),
        run: None,
        dispatch: Some(dispatch),
    }))
}

pub(super) fn run_is_a2a_resumable(run: &RunRecord) -> bool {
    run.is_resumable_waiting() && run.waiting_reason() != Some(WaitingReason::BackgroundTasks)
}

pub(super) async fn record_task_binding(
    st: &ProtocolRoutesState,
    thread_id: &str,
    task_id: &str,
    start_message_id: &str,
) -> Result<(), A2aError> {
    let (exists, mut thread) = load_thread_metadata_projection(st, thread_id).await?;
    let mut bindings = match thread.metadata.custom.remove(TASK_BINDINGS_METADATA_KEY) {
        Some(value) => decode_task_bindings_metadata(value)?,
        None => StoredTaskBindings::default(),
    };
    bindings.tasks.insert(
        task_id.to_string(),
        StoredTaskBinding {
            thread_id: thread_id.to_string(),
            start_message_id: start_message_id.to_string(),
            end_message_id: None,
        },
    );
    for (existing_task_id, binding) in bindings.tasks.iter_mut() {
        if existing_task_id != task_id && binding.end_message_id.is_none() {
            binding.end_message_id = Some(start_message_id.to_string());
        }
    }
    thread.metadata.custom.insert(
        TASK_BINDINGS_METADATA_KEY.to_string(),
        serde_json::to_value(bindings).map_err(|e| A2aError::Internal(e.to_string()))?,
    );
    persist_thread_metadata(st, thread_id, exists, thread).await?;
    Ok(())
}

async fn load_task_binding(
    st: &ProtocolRoutesState,
    thread_id: &str,
    task_id: &str,
) -> Result<Option<StoredTaskBinding>, A2aError> {
    let Some(thread) = st
        .run
        .store()
        .load_thread(thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
    else {
        return Ok(None);
    };

    let Some(value) = thread.metadata.custom.get(TASK_BINDINGS_METADATA_KEY) else {
        return Ok(None);
    };
    Ok(decode_task_bindings_metadata(value.clone())?
        .tasks
        .get(task_id)
        .cloned())
}

pub(super) fn decode_task_bindings_metadata(
    value: serde_json::Value,
) -> Result<StoredTaskBindings, A2aError> {
    serde_json::from_value::<StoredTaskBindings>(value).map_err(|error| {
        A2aError::Internal(format!(
            "corrupt A2A task binding metadata at {TASK_BINDINGS_METADATA_KEY}: {error}"
        ))
    })
}

pub(super) async fn task_context_id(
    st: &ProtocolRoutesState,
    task_id: &str,
) -> Result<String, A2aError> {
    Ok(resolve_task(st, task_id)
        .await?
        .map(|task| task.thread_id)
        .unwrap_or_else(|| task_id.to_string()))
}

pub(super) async fn wait_for_task(
    st: &ProtocolRoutesState,
    task_id: &str,
    tenant: Option<&str>,
    history_length: usize,
) -> Result<Task, A2aError> {
    let deadline = tokio::time::Instant::now() + BLOCKING_WAIT_TIMEOUT;
    let mut last_seen: Option<Task> = None;

    loop {
        if let Some(snapshot) =
            load_task_snapshot(st, task_id, tenant, history_length, true).await?
        {
            let state = snapshot.task.status.state;
            last_seen = Some(snapshot.task.clone());
            if state.is_terminal() || state.is_interrupted() {
                return Ok(snapshot.task);
            }
        }

        if tokio::time::Instant::now() >= deadline {
            let context_id = task_context_id(st, task_id).await?;
            return Ok(last_seen.unwrap_or_else(|| submitted_task(task_id, &context_id, tenant)));
        }

        tokio::time::sleep(BLOCKING_POLL_INTERVAL).await;
    }
}

pub(super) async fn load_task_snapshot(
    st: &ProtocolRoutesState,
    task_id: &str,
    tenant: Option<&str>,
    history_length: usize,
    include_artifacts: bool,
) -> Result<Option<TaskSnapshot>, A2aError> {
    let Some(task) = resolve_task(st, task_id).await? else {
        return Ok(None);
    };
    let thread_id = task.thread_id.clone();
    let latest_run = task.run.clone();
    let latest_dispatch: Option<RunDispatch> = if let Some(dispatch) = task.dispatch.clone() {
        Some(dispatch)
    } else {
        st.run
            .mailbox()
            .list_dispatches(&st.run.scoped_id(&thread_id), None, 100, 0)
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?
            .into_iter()
            .map(|dispatch| st.run.unscope_dispatch(dispatch))
            .filter(|dispatch| dispatch.run_id() == task_id || dispatch.dispatch_id() == task_id)
            .max_by_key(|dispatch| dispatch.updated_at())
    };

    let history = st
        .run
        .store()
        .load_messages(&thread_id)
        .await
        .map_err(|e| A2aError::Internal(e.to_string()))?
        .unwrap_or_default();
    let binding = load_task_binding(st, &thread_id, task_id).await?;
    let mut converted_history = if let Some(binding) = binding.as_ref()
        && !binding.start_message_id.is_empty()
    {
        let full_history = history
            .iter()
            .filter_map(|message| awaken_message_to_a2a_message(message, task_id, &thread_id))
            .collect::<Vec<_>>();
        let start_index = full_history
            .iter()
            .position(|message| message.message_id == binding.start_message_id)
            .unwrap_or(0);
        let end_index = binding
            .end_message_id
            .as_deref()
            .and_then(|message_id| {
                full_history
                    .iter()
                    .position(|message| message.message_id == message_id)
            })
            .unwrap_or(full_history.len());
        full_history
            .into_iter()
            .skip(start_index)
            .take(end_index.saturating_sub(start_index))
            .collect::<Vec<_>>()
    } else {
        history
            .iter()
            .filter_map(|message| awaken_message_to_a2a_message(message, task_id, &thread_id))
            .collect::<Vec<_>>()
    };
    let latest_agent_message = converted_history
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Agent)
        .cloned();

    let run_source = latest_run.as_ref().map(|record| TaskSource {
        state: run_record_to_task_state(record),
        updated_at_ms: record.updated_at.saturating_mul(1000),
        current_agent_id: Some(record.agent_id.clone()),
    });
    let dispatch_source = latest_dispatch.as_ref().map(|dispatch| TaskSource {
        state: dispatch_to_task_state(dispatch.status()),
        updated_at_ms: dispatch.updated_at(),
        current_agent_id: latest_run.as_ref().map(|record| record.agent_id.clone()),
    });

    let source = match (&run_source, &dispatch_source) {
        (Some(run), Some(dispatch)) if dispatch.updated_at_ms >= run.updated_at_ms => {
            if latest_dispatch
                .as_ref()
                .is_some_and(|dispatch| dispatch.status() != RunDispatchStatus::Acked)
            {
                dispatch_source
            } else {
                run_source
            }
        }
        (Some(_), _) => run_source,
        (_, Some(_)) => dispatch_source,
        (None, None) => None,
    };

    let Some(source) = source else {
        return Ok(None);
    };

    if let Some(tenant) = tenant
        && source.current_agent_id.as_deref() != Some(tenant)
    {
        return Ok(None);
    }

    if history_length != usize::MAX && converted_history.len() > history_length {
        let keep_from = converted_history.len().saturating_sub(history_length);
        converted_history = converted_history.split_off(keep_from);
    }

    let status_message = if matches!(
        source.state,
        TaskState::Completed
            | TaskState::Failed
            | TaskState::Rejected
            | TaskState::InputRequired
            | TaskState::AuthRequired
            | TaskState::Canceled
    ) {
        latest_agent_message.clone()
    } else {
        None
    };

    let artifacts = if include_artifacts && matches!(source.state, TaskState::Completed) {
        latest_agent_message
            .as_ref()
            .map(message_to_artifacts)
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(Some(TaskSnapshot {
        task: Task {
            id: task_id.to_string(),
            context_id: thread_id,
            status: TaskStatus {
                state: source.state,
                message: status_message,
                timestamp: None,
            },
            artifacts,
            history: converted_history,
            metadata: None,
        },
        updated_at_ms: source.updated_at_ms,
        current_agent_id: source.current_agent_id,
    }))
}

pub(super) fn run_record_to_task_state(record: &RunRecord) -> TaskState {
    match record.status {
        RunStatus::Created => TaskState::Submitted,
        RunStatus::Running => TaskState::Working,
        RunStatus::Waiting => match record.waiting_reason() {
            Some(WaitingReason::ToolPermission) => TaskState::AuthRequired,
            Some(WaitingReason::BackgroundTasks) => TaskState::Working,
            _ => TaskState::InputRequired,
        },
        RunStatus::Done => match record.termination_reason.as_ref() {
            Some(TerminationReason::Cancelled) => TaskState::Canceled,
            Some(TerminationReason::Blocked(_)) => TaskState::Rejected,
            Some(TerminationReason::Error(_)) => TaskState::Failed,
            _ => TaskState::Completed,
        },
    }
}

pub(super) fn dispatch_to_task_state(status: RunDispatchStatus) -> TaskState {
    match status {
        RunDispatchStatus::Queued => TaskState::Submitted,
        RunDispatchStatus::Claimed | RunDispatchStatus::Acked => TaskState::Working,
        RunDispatchStatus::Cancelled | RunDispatchStatus::Superseded => TaskState::Canceled,
        RunDispatchStatus::DeadLetter => TaskState::Failed,
    }
}

pub(super) fn submitted_task(task_id: &str, context_id: &str, tenant: Option<&str>) -> Task {
    Task {
        id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state: TaskState::Submitted,
            message: None,
            timestamp: None,
        },
        artifacts: Vec::new(),
        history: Vec::new(),
        metadata: tenant.map(|tenant| json!({"tenant": tenant})),
    }
}

pub(super) fn canceled_task(task_id: &str, context_id: &str, tenant: Option<&str>) -> Task {
    Task {
        id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state: TaskState::Canceled,
            message: None,
            timestamp: None,
        },
        artifacts: Vec::new(),
        history: Vec::new(),
        metadata: tenant.map(|tenant| json!({"tenant": tenant})),
    }
}

pub(super) async fn ensure_task_visible(
    st: &ProtocolRoutesState,
    task_id: &str,
    tenant: Option<&str>,
) -> Result<(), A2aError> {
    if let Some(tenant) = tenant {
        ensure_runnable_agent(st, tenant)?;
        let visible = load_task_snapshot(st, task_id, Some(tenant), 0, false)
            .await?
            .is_some();
        if !visible {
            return Err(A2aError::task_not_found(task_id.to_string()));
        }
        return Ok(());
    }

    if resolve_task(st, task_id).await?.is_some() {
        Ok(())
    } else {
        Err(A2aError::task_not_found(task_id.to_string()))
    }
}

pub(super) async fn collect_task_ids(st: &ProtocolRoutesState) -> Result<Vec<String>, A2aError> {
    let mut ids = BTreeSet::new();
    let mut run_offset = 0;
    loop {
        let page = st
            .run
            .store()
            .list_runs(&RunQuery {
                offset: run_offset,
                limit: 100,
                ..Default::default()
            })
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
        if page.items.is_empty() {
            break;
        }
        run_offset += page.items.len();
        ids.extend(page.items.into_iter().map(|run| run.run_id));
        if !page.has_more {
            break;
        }
    }

    let mut offset = 0;
    loop {
        let batch = st
            .run
            .store()
            .list_threads(offset, 100)
            .await
            .map_err(|e| A2aError::Internal(e.to_string()))?;
        if batch.is_empty() {
            break;
        }
        offset += batch.len();
        for thread_id in batch {
            let dispatches = st
                .run
                .mailbox()
                .list_dispatches(
                    &st.run.scoped_id(&thread_id),
                    Some(&[RunDispatchStatus::Queued, RunDispatchStatus::Claimed]),
                    100,
                    0,
                )
                .await
                .map_err(|e| A2aError::Internal(e.to_string()))?;
            ids.extend(
                dispatches
                    .into_iter()
                    .map(|dispatch| st.run.unscope_dispatch(dispatch).dispatch_id().clone()),
            );
        }
    }
    Ok(ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn decode_task_bindings_metadata_rejects_malformed_state() {
        let error = decode_task_bindings_metadata(json!({"tasks": []}))
            .expect_err("malformed task binding metadata must fail closed");
        match error {
            A2aError::Internal(message) => {
                assert!(message.contains(TASK_BINDINGS_METADATA_KEY));
                assert!(message.contains("invalid type"));
            }
            other => panic!("expected internal corruption error, got {other:?}"),
        }
    }
}
