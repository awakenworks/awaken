//! PostgreSQL canonical event subscription.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use awaken_contract::contract::event_store::{
    CanonicalEvent, EventCursor, EventPage, EventReader, EventScope, EventStoreError,
    EventSubscriber, SubscribeHandle, SubscribeStart,
};
use futures::StreamExt;
use sqlx::{PgPool, Row};

use crate::postgres::PostgresStore;

const SUBSCRIBE_PAGE_LIMIT: usize = 128;
const SUBSCRIBE_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct EventTables {
    scope_index: String,
}

impl EventTables {
    fn from_store(store: &PostgresStore) -> Self {
        let prefix = store
            .threads_table
            .strip_suffix("_threads")
            .unwrap_or(&store.threads_table);
        Self {
            scope_index: format!("{prefix}_event_scope_index"),
        }
    }
}

struct SubscribeState {
    store: PostgresStore,
    scope: EventScope,
    cursor: Option<EventCursor>,
    pending: VecDeque<CanonicalEvent>,
    sleep_after_empty: bool,
}

#[async_trait]
impl EventSubscriber for PostgresStore {
    async fn subscribe(
        &self,
        scope: EventScope,
        start: SubscribeStart,
    ) -> Result<SubscribeHandle, EventStoreError> {
        self.ensure_schema()
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        let start_cursor = match &start {
            SubscribeStart::FromStart => None,
            SubscribeStart::FromCursor(cursor) => Some(cursor.clone()),
            SubscribeStart::FromNow => {
                last_cursor(&self.pool, &EventTables::from_store(self), &scope).await?
            }
        };
        let cursor = match start {
            SubscribeStart::FromStart => None,
            SubscribeStart::FromCursor(cursor) => Some(cursor),
            SubscribeStart::FromNow => start_cursor.clone(),
        };
        let state = SubscribeState {
            store: self.clone(),
            scope,
            cursor,
            pending: VecDeque::new(),
            sleep_after_empty: false,
        };
        let stream = futures::stream::unfold(state, next_subscription_item).boxed();
        Ok(SubscribeHandle {
            start_cursor,
            stream,
        })
    }
}

async fn next_subscription_item(
    mut state: SubscribeState,
) -> Option<(Result<CanonicalEvent, EventStoreError>, SubscribeState)> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            state.cursor = event.cursors_by_scope.get(&state.scope).cloned();
            return Some((Ok(event), state));
        }
        if state.sleep_after_empty {
            tokio::time::sleep(SUBSCRIBE_POLL_INTERVAL).await;
            state.sleep_after_empty = false;
        }
        match state
            .store
            .list(
                state.scope.clone(),
                state.cursor.clone(),
                SUBSCRIBE_PAGE_LIMIT,
            )
            .await
        {
            Ok(EventPage { events, .. }) if events.is_empty() => {
                state.sleep_after_empty = true;
            }
            Ok(EventPage { events, .. }) => {
                state.pending = events.into();
            }
            Err(error) => return Some((Err(error), state)),
        }
    }
}

async fn last_cursor(
    pool: &PgPool,
    tables: &EventTables,
    scope: &EventScope,
) -> Result<Option<EventCursor>, EventStoreError> {
    let sql = format!(
        "SELECT cursor
         FROM {}
         WHERE scope_key = $1
         ORDER BY sequence DESC
         LIMIT 1",
        tables.scope_index
    );
    let row = sqlx::query(&sql)
        .bind(scope_key(scope)?)
        .fetch_optional(pool)
        .await
        .map_err(|error| EventStoreError::Io(error.to_string()))?;
    row.map(|row| {
        EventCursor::new(
            row.try_get::<String, _>("cursor")
                .map_err(|error| EventStoreError::Io(error.to_string()))?,
        )
    })
    .transpose()
}

fn scope_key(scope: &EventScope) -> Result<String, EventStoreError> {
    serde_json::to_string(scope).map_err(|error| EventStoreError::Serialization(error.to_string()))
}

#[cfg(test)]
mod tests {
    use awaken_contract::contract::event_store::EventStore;

    use super::*;

    #[test]
    fn postgres_store_implements_full_event_store_trait() {
        fn assert_event_store<T: EventStore>() {}
        assert_event_store::<PostgresStore>();
    }
}
