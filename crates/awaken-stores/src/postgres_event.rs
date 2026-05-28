//! PostgreSQL canonical event-store implementation.

use std::collections::BTreeMap;

use async_trait::async_trait;
use awaken_server_contract::contract::event_store::{
    AppendOptions, AppendResult, CanonicalEvent, CanonicalEventDraft, CanonicalEventId,
    CanonicalEventKind, EventCursor, EventPage, EventReader, EventScope, EventStoreError,
    EventVisibility, EventWriter,
};
use awaken_server_contract::contract::storage::StorageError;
use sqlx::{Postgres, Row, Transaction};

use crate::postgres::PostgresStore;

struct EventTables {
    events: String,
    scope_index: String,
    counters: String,
    idempotency: String,
}

impl EventTables {
    fn from_store(store: &PostgresStore) -> Self {
        let prefix = store
            .threads_table
            .strip_suffix("_threads")
            .unwrap_or(&store.threads_table);
        Self {
            events: format!("{prefix}_events"),
            scope_index: format!("{prefix}_event_scope_index"),
            counters: format!("{prefix}_event_scope_counters"),
            idempotency: format!("{prefix}_event_idempotency"),
        }
    }
}

pub(crate) async fn ensure_event_schema(store: &PostgresStore) -> Result<(), StorageError> {
    let tables = EventTables::from_store(store);
    let statements = vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                event_id TEXT PRIMARY KEY,
                scopes JSONB NOT NULL,
                event_kind TEXT NOT NULL,
                payload JSONB NOT NULL,
                thread_id TEXT,
                run_id TEXT,
                causation_id TEXT,
                correlation_id TEXT,
                origin TEXT NOT NULL,
                visibility TEXT NOT NULL,
                schema_version INTEGER NOT NULL CHECK (schema_version > 0),
                created_at BIGINT NOT NULL
            )",
            tables.events
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                scope_key TEXT PRIMARY KEY,
                scope_json JSONB NOT NULL,
                last_sequence BIGINT NOT NULL CHECK (last_sequence > 0),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
            tables.counters
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                scope_key TEXT NOT NULL,
                scope_json JSONB NOT NULL,
                scope_type TEXT NOT NULL,
                scope_id TEXT NOT NULL,
                thread_id TEXT,
                run_id TEXT,
                sequence BIGINT NOT NULL CHECK (sequence > 0),
                cursor TEXT NOT NULL,
                event_id TEXT NOT NULL REFERENCES {} (event_id) ON DELETE CASCADE,
                created_at BIGINT NOT NULL,
                PRIMARY KEY (scope_key, sequence),
                UNIQUE (scope_key, cursor),
                UNIQUE (scope_key, event_id)
            )",
            tables.scope_index, tables.events
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                writer_id TEXT NOT NULL,
                idempotency_key TEXT NOT NULL,
                append_digest BYTEA NOT NULL,
                event_id TEXT NOT NULL REFERENCES {} (event_id) ON DELETE CASCADE,
                created_at BIGINT NOT NULL,
                PRIMARY KEY (writer_id, idempotency_key)
            )",
            tables.idempotency, tables.events
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_{}_thread_created ON {} (thread_id, created_at DESC) WHERE thread_id IS NOT NULL",
            tables.events, tables.events
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_{}_run_created ON {} (run_id, created_at DESC) WHERE run_id IS NOT NULL",
            tables.events, tables.events
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_{}_event_id ON {} (event_id)",
            tables.scope_index, tables.scope_index
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_{}_scope_type_id_seq ON {} (scope_type, scope_id, sequence)",
            tables.scope_index, tables.scope_index
        ),
    ];

    for stmt in statements {
        sqlx::query(&stmt)
            .execute(&store.pool)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
    }
    Ok(())
}

impl PostgresStore {
    /// Body of [`EventWriter::append`] parameterised on an externally-managed
    /// transaction (ADR-0036 D2). The caller opens the transaction, commits
    /// on success, and rolls back on failure. The canonical outbox row is
    /// inserted in the same transaction (ADR-0034 D9).
    pub(crate) async fn append_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        draft: CanonicalEventDraft,
        options: AppendOptions,
    ) -> Result<AppendResult, EventStoreError> {
        draft.validate()?;
        options.validate()?;
        validate_expected_cursor_scopes(&draft, &options)?;
        let append_digest = draft.idempotency_digest()?;
        let idempotency_identity = options.idempotency_identity()?;

        if let Some((writer_id, idempotency_key)) = idempotency_identity.as_ref() {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
                .bind(writer_id)
                .bind(idempotency_key)
                .execute(&mut **tx)
                .await
                .map_err(|error| EventStoreError::Io(error.to_string()))?;

            if let Some((stored_digest, event_id)) = self
                .load_idempotency_tx(tx, writer_id, idempotency_key)
                .await?
            {
                if stored_digest != append_digest {
                    return Err(EventStoreError::IdempotencyConflict(format!(
                        "idempotency identity reused with different input: writer_id={writer_id}, idempotency_key={idempotency_key}"
                    )));
                }
                let event = self.load_event_by_id_tx(tx, &event_id).await?;
                return Ok(AppendResult { event });
            }
        }

        self.validate_expected_prior_cursors_tx(tx, &options)
            .await?;
        let event_id = CanonicalEventId::new(format!("evt_{}", uuid::Uuid::now_v7()))?;
        let created_at = current_millis_i64()?;
        let mut cursors_by_scope = BTreeMap::new();
        for scope in &draft.scopes {
            let sequence = self.next_scope_sequence_tx(tx, scope).await?;
            cursors_by_scope.insert(
                scope.clone(),
                EventCursor::new(format!("cur_pg_{sequence}"))?,
            );
        }

        let event = CanonicalEvent::from_append(
            event_id.clone(),
            cursors_by_scope.clone(),
            created_at as u64,
            draft,
        )?;
        self.insert_event_tx(tx, &event).await?;
        for scope in &event.scopes {
            let cursor = event.cursors_by_scope.get(scope).ok_or_else(|| {
                EventStoreError::Integrity("persisted event missing scope cursor".to_string())
            })?;
            self.insert_scope_index_tx(tx, &event, scope, cursor)
                .await?;
        }
        if let Some((writer_id, idempotency_key)) = idempotency_identity.as_ref() {
            self.insert_idempotency_tx(
                tx,
                writer_id,
                idempotency_key,
                &append_digest,
                &event.event_id,
                created_at,
            )
            .await?;
        }
        crate::postgres_outbox::insert_canonical_event_outbox_tx(self, tx, &event, created_at)
            .await?;
        Ok(AppendResult { event })
    }
}

#[async_trait]
impl EventWriter for PostgresStore {
    async fn append(
        &self,
        draft: CanonicalEventDraft,
        options: AppendOptions,
    ) -> Result<AppendResult, EventStoreError> {
        self.ensure_schema()
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        let result = self.append_in_tx(&mut tx, draft, options).await?;
        tx.commit()
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        Ok(result)
    }
}

#[async_trait]
impl EventReader for PostgresStore {
    async fn list(
        &self,
        scope: EventScope,
        from: Option<EventCursor>,
        limit: usize,
    ) -> Result<EventPage, EventStoreError> {
        self.ensure_schema()
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        let scope_key = scope_key(&scope)?;
        let tables = self.event_tables();
        let start_sequence = match from.as_ref() {
            Some(cursor) => Some(self.cursor_sequence(&scope_key, cursor).await?),
            None => None,
        };
        let fetch_limit = limit.saturating_add(1);
        let sql = format!(
            "SELECT event_id
             FROM {}
             WHERE scope_key = $1 AND sequence > $2
             ORDER BY sequence ASC
             LIMIT $3",
            tables.scope_index
        );
        let rows = sqlx::query(&sql)
            .bind(&scope_key)
            .bind(start_sequence.unwrap_or(0))
            .bind(fetch_limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        let has_more = rows.len() > limit;
        let rows = if has_more { &rows[..limit] } else { &rows[..] };
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let event_id: String = row
                .try_get("event_id")
                .map_err(|error| EventStoreError::Io(error.to_string()))?;
            events.push(self.load_event_by_id(&event_id).await?);
        }
        let next_cursor = if has_more {
            events
                .last()
                .and_then(|event| event.cursors_by_scope.get(&scope))
                .cloned()
        } else {
            None
        };
        Ok(EventPage {
            events,
            next_cursor,
            has_more,
        })
    }

    async fn count(&self, scope: EventScope) -> Result<u64, EventStoreError> {
        self.ensure_schema()
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        let scope_key = scope_key(&scope)?;
        let tables = self.event_tables();
        let sql = format!(
            "SELECT COUNT(*)::BIGINT AS count FROM {} WHERE scope_key = $1",
            tables.scope_index
        );
        let row = sqlx::query(&sql)
            .bind(scope_key)
            .fetch_one(&self.pool)
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        let count: i64 = row
            .try_get("count")
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        u64::try_from(count).map_err(|error| EventStoreError::Integrity(error.to_string()))
    }
}

impl PostgresStore {
    fn event_tables(&self) -> EventTables {
        EventTables::from_store(self)
    }

    async fn load_idempotency_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        writer_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<(Vec<u8>, String)>, EventStoreError> {
        let tables = self.event_tables();
        let sql = format!(
            "SELECT append_digest, event_id
             FROM {}
             WHERE writer_id = $1 AND idempotency_key = $2",
            tables.idempotency
        );
        sqlx::query(&sql)
            .bind(writer_id)
            .bind(idempotency_key)
            .fetch_optional(&mut **tx)
            .await
            .map(|row| {
                row.map(|row| {
                    let digest = row.try_get::<Vec<u8>, _>("append_digest")?;
                    let event_id = row.try_get::<String, _>("event_id")?;
                    Ok((digest, event_id))
                })
                .transpose()
            })
            .map_err(|error| EventStoreError::Io(error.to_string()))?
            .map_err(|error: sqlx::Error| EventStoreError::Io(error.to_string()))
    }

    async fn validate_expected_prior_cursors_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        options: &AppendOptions,
    ) -> Result<(), EventStoreError> {
        for (scope, expected_cursor) in &options.expected_prior_cursors {
            let scope_key = scope_key(scope)?;
            let actual = self.last_cursor_tx(tx, &scope_key).await?;
            if actual.as_ref() != Some(expected_cursor) {
                return Err(EventStoreError::ExpectedCursorConflict(format!(
                    "expected prior cursor mismatch for scope {scope:?}"
                )));
            }
        }
        Ok(())
    }

    async fn last_cursor_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        scope_key: &str,
    ) -> Result<Option<EventCursor>, EventStoreError> {
        let tables = self.event_tables();
        let sql = format!(
            "SELECT cursor
             FROM {}
             WHERE scope_key = $1
             ORDER BY sequence DESC
             LIMIT 1",
            tables.scope_index
        );
        let row = sqlx::query(&sql)
            .bind(scope_key)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        row.map(|row| {
            let cursor: String = row
                .try_get("cursor")
                .map_err(|error| EventStoreError::Io(error.to_string()))?;
            EventCursor::new(cursor)
        })
        .transpose()
    }

    async fn next_scope_sequence_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        scope: &EventScope,
    ) -> Result<i64, EventStoreError> {
        let tables = self.event_tables();
        let scope_key = scope_key(scope)?;
        let scope_json = serde_json::to_value(scope)
            .map_err(|error| EventStoreError::Serialization(error.to_string()))?;
        let sql = format!(
            "INSERT INTO {} (scope_key, scope_json, last_sequence)
             VALUES ($1, $2, 1)
             ON CONFLICT (scope_key) DO UPDATE SET
                 last_sequence = {}.last_sequence + 1,
                 scope_json = EXCLUDED.scope_json,
                 updated_at = now()
             RETURNING last_sequence",
            tables.counters, tables.counters
        );
        let row = sqlx::query(&sql)
            .bind(scope_key)
            .bind(scope_json)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        row.try_get("last_sequence")
            .map_err(|error| EventStoreError::Io(error.to_string()))
    }

    async fn insert_event_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        event: &CanonicalEvent,
    ) -> Result<(), EventStoreError> {
        let tables = self.event_tables();
        let sql = format!(
            "INSERT INTO {} (
                event_id, scopes, event_kind, payload, thread_id, run_id,
                causation_id, correlation_id, origin, visibility, schema_version, created_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            tables.events
        );
        let scopes = serde_json::to_value(&event.scopes)
            .map_err(|error| EventStoreError::Serialization(error.to_string()))?;
        sqlx::query(&sql)
            .bind(event.event_id.as_str())
            .bind(scopes)
            .bind(event.event_kind.as_str())
            .bind(&event.payload)
            .bind(event.thread_id.as_deref())
            .bind(event.run_id.as_deref())
            .bind(event.causation_id.as_deref())
            .bind(event.correlation_id.as_deref())
            .bind(&event.origin)
            .bind(visibility_to_str(event.visibility))
            .bind(i32::try_from(event.schema_version).map_err(|error| {
                EventStoreError::Validation(format!("schema_version out of range: {error}"))
            })?)
            .bind(i64::try_from(event.created_at).map_err(|error| {
                EventStoreError::Validation(format!("created_at out of range: {error}"))
            })?)
            .execute(&mut **tx)
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        Ok(())
    }

    async fn insert_scope_index_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        event: &CanonicalEvent,
        scope: &EventScope,
        cursor: &EventCursor,
    ) -> Result<(), EventStoreError> {
        let tables = self.event_tables();
        let parts = ScopeParts::from_scope(scope)?;
        let scope_json = serde_json::to_value(scope)
            .map_err(|error| EventStoreError::Serialization(error.to_string()))?;
        let sequence = cursor_sequence_from_cursor(cursor)?;
        let sql = format!(
            "INSERT INTO {} (
                scope_key, scope_json, scope_type, scope_id, thread_id, run_id,
                sequence, cursor, event_id, created_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            tables.scope_index
        );
        sqlx::query(&sql)
            .bind(parts.scope_key)
            .bind(scope_json)
            .bind(parts.scope_type)
            .bind(parts.scope_id)
            .bind(parts.thread_id)
            .bind(parts.run_id)
            .bind(sequence)
            .bind(cursor.as_str())
            .bind(event.event_id.as_str())
            .bind(i64::try_from(event.created_at).map_err(|error| {
                EventStoreError::Validation(format!("created_at out of range: {error}"))
            })?)
            .execute(&mut **tx)
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        Ok(())
    }

    async fn insert_idempotency_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        writer_id: &str,
        idempotency_key: &str,
        append_digest: &[u8],
        event_id: &CanonicalEventId,
        created_at: i64,
    ) -> Result<(), EventStoreError> {
        let tables = self.event_tables();
        let sql = format!(
            "INSERT INTO {} (writer_id, idempotency_key, append_digest, event_id, created_at)
             VALUES ($1, $2, $3, $4, $5)",
            tables.idempotency
        );
        sqlx::query(&sql)
            .bind(writer_id)
            .bind(idempotency_key)
            .bind(append_digest)
            .bind(event_id.as_str())
            .bind(created_at)
            .execute(&mut **tx)
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        Ok(())
    }

    async fn cursor_sequence(
        &self,
        scope_key: &str,
        cursor: &EventCursor,
    ) -> Result<i64, EventStoreError> {
        let tables = self.event_tables();
        let sql = format!(
            "SELECT sequence
             FROM {}
             WHERE scope_key = $1 AND cursor = $2",
            tables.scope_index
        );
        let row = sqlx::query(&sql)
            .bind(scope_key)
            .bind(cursor.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        row.map(|row| {
            row.try_get("sequence")
                .map_err(|error| EventStoreError::Io(error.to_string()))
        })
        .transpose()?
        .ok_or_else(|| EventStoreError::CursorExpired(cursor.as_str().to_string()))
    }
    pub(crate) async fn load_event_by_id(
        &self,
        event_id: &str,
    ) -> Result<CanonicalEvent, EventStoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        let event = self.load_event_by_id_tx(&mut tx, event_id).await?;
        tx.commit()
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        Ok(event)
    }
    async fn load_event_by_id_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        event_id: &str,
    ) -> Result<CanonicalEvent, EventStoreError> {
        let tables = self.event_tables();
        let sql = format!(
            "SELECT event_id, scopes, event_kind, payload, causation_id, correlation_id,
                    origin, visibility, schema_version, created_at
             FROM {}
             WHERE event_id = $1",
            tables.events
        );
        let row = sqlx::query(&sql)
            .bind(event_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?
            .ok_or_else(|| EventStoreError::Integrity(format!("missing event: {event_id}")))?;
        let scopes: Vec<EventScope> = serde_json::from_value(
            row.try_get("scopes")
                .map_err(|error| EventStoreError::Io(error.to_string()))?,
        )
        .map_err(|error| EventStoreError::Serialization(error.to_string()))?;
        let mut draft = CanonicalEventDraft::new(
            scopes,
            CanonicalEventKind::new(
                row.try_get::<String, _>("event_kind")
                    .map_err(|error| EventStoreError::Io(error.to_string()))?,
            )?,
            row.try_get("payload")
                .map_err(|error| EventStoreError::Io(error.to_string()))?,
            row.try_get::<String, _>("origin")
                .map_err(|error| EventStoreError::Io(error.to_string()))?,
        )?;
        draft.causation_id = row
            .try_get("causation_id")
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        draft.correlation_id = row
            .try_get("correlation_id")
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        let visibility: String = row
            .try_get("visibility")
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        draft.visibility = visibility_from_str(&visibility)?;
        let schema_version: i32 = row
            .try_get("schema_version")
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        draft.schema_version = u32::try_from(schema_version)
            .map_err(|error| EventStoreError::Integrity(error.to_string()))?;
        let cursors_by_scope = self.load_cursors_by_scope_tx(tx, event_id).await?;
        let created_at: i64 = row
            .try_get("created_at")
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        CanonicalEvent::from_append(
            CanonicalEventId::new(
                row.try_get::<String, _>("event_id")
                    .map_err(|error| EventStoreError::Io(error.to_string()))?,
            )?,
            cursors_by_scope,
            u64::try_from(created_at)
                .map_err(|error| EventStoreError::Integrity(error.to_string()))?,
            draft,
        )
    }

    async fn load_cursors_by_scope_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        event_id: &str,
    ) -> Result<BTreeMap<EventScope, EventCursor>, EventStoreError> {
        let tables = self.event_tables();
        let sql = format!(
            "SELECT scope_json, cursor
             FROM {}
             WHERE event_id = $1",
            tables.scope_index
        );
        let rows = sqlx::query(&sql)
            .bind(event_id)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        let mut cursors = BTreeMap::new();
        for row in rows {
            let scope: EventScope = serde_json::from_value(
                row.try_get("scope_json")
                    .map_err(|error| EventStoreError::Io(error.to_string()))?,
            )
            .map_err(|error| EventStoreError::Serialization(error.to_string()))?;
            let cursor = EventCursor::new(
                row.try_get::<String, _>("cursor")
                    .map_err(|error| EventStoreError::Io(error.to_string()))?,
            )?;
            cursors.insert(scope, cursor);
        }
        Ok(cursors)
    }
}
#[derive(Debug)]
struct ScopeParts {
    scope_key: String,
    scope_type: &'static str,
    scope_id: String,
    thread_id: Option<String>,
    run_id: Option<String>,
}

impl ScopeParts {
    fn from_scope(scope: &EventScope) -> Result<Self, EventStoreError> {
        let scope_key = scope_key(scope)?;
        Ok(match scope {
            EventScope::Thread { thread_id } => Self {
                scope_key,
                scope_type: "thread",
                scope_id: thread_id.clone(),
                thread_id: Some(thread_id.clone()),
                run_id: None,
            },
            EventScope::Run { run_id } => Self {
                scope_key,
                scope_type: "run",
                scope_id: run_id.clone(),
                thread_id: None,
                run_id: Some(run_id.clone()),
            },
        })
    }
}

fn validate_expected_cursor_scopes(
    draft: &CanonicalEventDraft,
    options: &AppendOptions,
) -> Result<(), EventStoreError> {
    for expected_scope in options.expected_prior_cursors.keys() {
        if !draft.scopes.iter().any(|scope| scope == expected_scope) {
            return Err(EventStoreError::Validation(format!(
                "expected cursor scope is not in append scope set: {expected_scope:?}"
            )));
        }
    }
    Ok(())
}

fn scope_key(scope: &EventScope) -> Result<String, EventStoreError> {
    serde_json::to_string(scope).map_err(|error| EventStoreError::Serialization(error.to_string()))
}

fn cursor_sequence_from_cursor(cursor: &EventCursor) -> Result<i64, EventStoreError> {
    cursor
        .as_str()
        .strip_prefix("cur_pg_")
        .ok_or_else(|| EventStoreError::Integrity("unexpected postgres cursor".to_string()))?
        .parse::<i64>()
        .map_err(|error| EventStoreError::Integrity(error.to_string()))
}

fn current_millis_i64() -> Result<i64, EventStoreError> {
    i64::try_from(crate::current_millis())
        .map_err(|error| EventStoreError::Validation(format!("current time out of range: {error}")))
}

fn visibility_to_str(visibility: EventVisibility) -> &'static str {
    match visibility {
        EventVisibility::Public => "public",
        EventVisibility::Internal => "internal",
        EventVisibility::Audit => "audit",
        EventVisibility::Sensitive => "sensitive",
    }
}

fn visibility_from_str(value: &str) -> Result<EventVisibility, EventStoreError> {
    match value {
        "public" => Ok(EventVisibility::Public),
        "internal" => Ok(EventVisibility::Internal),
        "audit" => Ok(EventVisibility::Audit),
        "sensitive" => Ok(EventVisibility::Sensitive),
        other => Err(EventStoreError::Integrity(format!(
            "unknown event visibility: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_key_distinguishes_scope_families() {
        let thread = scope_key(&EventScope::thread("same")).unwrap();
        let run = scope_key(&EventScope::run("same")).unwrap();
        assert_ne!(thread, run);
    }

    #[test]
    fn scope_parts_for_thread_preserve_query_columns() {
        let parts = ScopeParts::from_scope(&EventScope::thread("thread")).unwrap();
        assert_eq!(parts.scope_type, "thread");
        assert_eq!(parts.scope_id, "thread");
        assert_eq!(parts.thread_id.as_deref(), Some("thread"));
        assert_eq!(parts.run_id, None);
    }

    #[test]
    fn visibility_round_trip_matches_contract_wire_values() {
        for visibility in [
            EventVisibility::Public,
            EventVisibility::Internal,
            EventVisibility::Audit,
            EventVisibility::Sensitive,
        ] {
            assert_eq!(
                visibility_from_str(visibility_to_str(visibility)).unwrap(),
                visibility
            );
        }
    }
}
