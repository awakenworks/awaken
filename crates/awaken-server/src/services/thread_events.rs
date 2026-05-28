use awaken_server_contract::contract::event_store::{
    AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventScope, EventStoreError,
    EventWriter,
};
use awaken_server_contract::contract::storage::{ChildThreadDeleteStrategy, StorageError};
use awaken_server_contract::thread::Thread;
use serde_json::{Value, json};

use crate::app::RunRoutesState;
use crate::services::thread_service::{
    CreateThreadOptions, DeleteThreadOptions, UpdateThreadOptions,
};

pub(crate) async fn create_thread(
    state: &RunRoutesState,
    options: CreateThreadOptions,
) -> Result<Thread, StorageError> {
    let thread = crate::services::thread_service::create_thread_with_options(
        state.run.store().as_ref(),
        options,
    )
    .await?;
    record_thread_created(state, &thread).await;
    Ok(thread)
}

pub(crate) async fn update_thread(
    state: &RunRoutesState,
    thread_id: &str,
    options: UpdateThreadOptions,
) -> Result<Thread, StorageError> {
    let before = state
        .run
        .store()
        .load_thread(thread_id)
        .await?
        .ok_or_else(|| StorageError::NotFound(thread_id.to_string()))?;
    let thread = crate::services::thread_service::update_thread(
        state.run.store().as_ref(),
        thread_id,
        options,
    )
    .await?;
    record_thread_updated(state, &before, &thread).await;
    Ok(thread)
}

pub(crate) async fn delete_thread(
    state: &RunRoutesState,
    thread_id: &str,
    options: DeleteThreadOptions,
) -> Result<(), StorageError> {
    let thread = state
        .run
        .store()
        .load_thread(thread_id)
        .await?
        .ok_or_else(|| StorageError::NotFound(thread_id.to_string()))?;
    crate::services::thread_service::delete_thread(state.run.store().as_ref(), thread_id, options)
        .await?;
    record_thread_deleted(state, &thread, options.child_strategy).await;
    Ok(())
}

pub(crate) async fn record_thread_created(state: &RunRoutesState, thread: &Thread) {
    record_thread_event(
        state,
        "ThreadCreated",
        thread,
        thread_payload(thread, None, None),
        Some(format!("ThreadCreated/{}", thread.id)),
    )
    .await;
}

pub(crate) async fn record_thread_updated(state: &RunRoutesState, before: &Thread, after: &Thread) {
    record_thread_event(
        state,
        "ThreadUpdated",
        after,
        thread_payload(after, Some(before), None),
        None,
    )
    .await;
}

pub(crate) async fn record_thread_deleted(
    state: &RunRoutesState,
    thread: &Thread,
    child_strategy: ChildThreadDeleteStrategy,
) {
    record_thread_event(
        state,
        "ThreadDeleted",
        thread,
        thread_payload(thread, None, Some(child_strategy)),
        Some(format!("ThreadDeleted/{}", thread.id)),
    )
    .await;
}

async fn record_thread_event(
    state: &RunRoutesState,
    event_kind: &'static str,
    thread: &Thread,
    payload: Value,
    idempotency_key: Option<String>,
) {
    let Some(writer) = state
        .events
        .as_ref()
        .map(|events| events.event_store.clone())
    else {
        return;
    };
    if let Err(error) = append_thread_event(
        writer.as_ref(),
        event_kind,
        thread,
        payload,
        idempotency_key,
    )
    .await
    {
        tracing::error!(error = %error, thread_id = %thread.id, event_kind, "failed to record thread event");
    }
}

async fn append_thread_event(
    writer: &dyn EventWriter,
    event_kind: &'static str,
    thread: &Thread,
    payload: Value,
    idempotency_key: Option<String>,
) -> Result<(), EventStoreError> {
    let mut draft = CanonicalEventDraft::new(
        scopes_for_thread(thread),
        CanonicalEventKind::new(event_kind)?,
        payload,
        "server",
    )?;
    draft.correlation_id = Some(thread.id.clone());
    writer
        .append(
            draft,
            AppendOptions {
                writer_id: Some("thread-service".to_string()),
                idempotency_key,
                expected_prior_cursors: Default::default(),
            },
        )
        .await?;
    Ok(())
}

fn scopes_for_thread(thread: &Thread) -> Vec<EventScope> {
    vec![EventScope::thread(thread.id.clone())]
}

fn thread_payload(
    thread: &Thread,
    before: Option<&Thread>,
    child_strategy: Option<ChildThreadDeleteStrategy>,
) -> Value {
    let mut payload = json!({
        "thread_id": thread.id,
        "resource_id": thread.resource_id,
        "parent_thread_id": thread.parent_thread_id,
        "title": thread.metadata.title,
        "created_at": thread.metadata.created_at,
        "updated_at": thread.metadata.updated_at,
    });
    if let Some(before) = before
        && let Some(map) = payload.as_object_mut()
    {
        map.insert(
            "previous".to_string(),
            json!({
                "resource_id": before.resource_id,
                "parent_thread_id": before.parent_thread_id,
                "title": before.metadata.title,
                "updated_at": before.metadata.updated_at,
            }),
        );
    }
    if let Some(child_strategy) = child_strategy
        && let Some(map) = payload.as_object_mut()
    {
        map.insert("child_strategy".to_string(), json!(child_strategy));
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::contract::event_store::{EventReader, EventVisibility};
    use awaken_stores::InMemoryEventStore;

    fn child_thread() -> Thread {
        let mut thread = Thread::with_id("thread-1")
            .with_title("Child")
            .with_resource_id("resource-a")
            .with_parent_thread_id("parent-1");
        thread.metadata.created_at = Some(10);
        thread.metadata.updated_at = Some(20);
        thread
    }

    #[tokio::test]
    async fn append_thread_event_indexes_thread_scope() {
        let store = InMemoryEventStore::new();
        append_thread_event(
            &store,
            "ThreadCreated",
            &child_thread(),
            thread_payload(&child_thread(), None, None),
            Some("ThreadCreated/thread-1".to_string()),
        )
        .await
        .unwrap();

        let by_thread = store
            .list(EventScope::thread("thread-1"), None, 10)
            .await
            .unwrap();

        assert_eq!(by_thread.events.len(), 1);
        assert_eq!(by_thread.events[0].event_kind.as_str(), "ThreadCreated");
        assert_eq!(by_thread.events[0].payload["resource_id"], "resource-a");
        assert_eq!(by_thread.events[0].payload["parent_thread_id"], "parent-1");
        assert_eq!(by_thread.events[0].visibility, EventVisibility::Public);
    }

    #[tokio::test]
    async fn thread_update_payload_carries_previous_lineage() {
        let store = InMemoryEventStore::new();
        let before = child_thread();
        let mut after = child_thread();
        after.resource_id = Some("resource-b".to_string());
        after.parent_thread_id = Some("parent-2".to_string());
        after.metadata.updated_at = Some(30);

        append_thread_event(
            &store,
            "ThreadUpdated",
            &after,
            thread_payload(&after, Some(&before), None),
            None,
        )
        .await
        .unwrap();

        let page = store
            .list(EventScope::thread("thread-1"), None, 10)
            .await
            .unwrap();
        assert_eq!(page.events[0].payload["resource_id"], "resource-b");
        assert_eq!(page.events[0].payload["parent_thread_id"], "parent-2");
        assert_eq!(
            page.events[0].payload["previous"]["resource_id"],
            "resource-a"
        );
        assert_eq!(
            page.events[0].payload["previous"]["parent_thread_id"],
            "parent-1"
        );
    }
}
