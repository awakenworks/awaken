//! PostgreSQL protocol replay-log implementation.

use async_trait::async_trait;
use awaken_contract::OutboxError;
use awaken_contract::contract::protocol_replay_log::{
    ProtocolReplayAppendResult, ProtocolReplayCursor, ProtocolReplayDraft, ProtocolReplayError,
    ProtocolReplayId, ProtocolReplayLookup, ProtocolReplayPage, ProtocolReplayReader,
    ProtocolReplayRecord, ProtocolReplayRedactionState, ProtocolReplayWriter, ProtocolStreamKey,
};
use awaken_contract::contract::storage::StorageError;
use sqlx::{Postgres, Row, Transaction};

use crate::postgres::PostgresStore;

struct ProtocolReplayTables {
    log: String,
    counters: String,
}

impl ProtocolReplayTables {
    fn from_store(store: &PostgresStore) -> Self {
        let prefix = store
            .threads_table
            .strip_suffix("_threads")
            .unwrap_or(&store.threads_table);
        Self {
            log: format!("{prefix}_protocol_replay_log"),
            counters: format!("{prefix}_protocol_replay_counters"),
        }
    }
}

pub(crate) async fn ensure_protocol_replay_schema(
    store: &PostgresStore,
) -> Result<(), StorageError> {
    let tables = ProtocolReplayTables::from_store(store);
    let statements = vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                protocol_replay_id TEXT PRIMARY KEY,
                stream_id TEXT NOT NULL,
                scope_key TEXT NOT NULL,
                scope_json JSONB NOT NULL,
                scope_type TEXT NOT NULL,
                scope_id TEXT NOT NULL,
                protocol TEXT NOT NULL,
                protocol_version TEXT NOT NULL,
                projector_version TEXT NOT NULL,
                protocol_replay_sequence BIGINT NOT NULL CHECK (protocol_replay_sequence > 0),
                protocol_replay_cursor TEXT NOT NULL,
                wire_event_id TEXT NOT NULL,
                wire_event_type TEXT NOT NULL,
                wire_payload_bytes BYTEA NOT NULL,
                wire_payload_json JSONB,
                source_event_ids JSONB NOT NULL,
                source_event_cursors JSONB NOT NULL,
                redaction_state TEXT NOT NULL,
                created_at BIGINT NOT NULL,
                expires_at BIGINT,
                UNIQUE (stream_id, protocol, protocol_version, protocol_replay_sequence),
                UNIQUE (stream_id, protocol, protocol_version, protocol_replay_cursor),
                UNIQUE (protocol, protocol_version, wire_event_id)
            )",
            tables.log
        ),
        format!(
            "ALTER TABLE {} ADD COLUMN IF NOT EXISTS stream_id TEXT",
            tables.log
        ),
        format!(
            "UPDATE {} SET stream_id = scope_key WHERE stream_id IS NULL",
            tables.log
        ),
        format!(
            "ALTER TABLE {} ALTER COLUMN stream_id SET NOT NULL",
            tables.log
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                stream_key TEXT PRIMARY KEY,
                scope_key TEXT NOT NULL,
                scope_json JSONB NOT NULL,
                protocol TEXT NOT NULL,
                protocol_version TEXT NOT NULL,
                last_sequence BIGINT NOT NULL CHECK (last_sequence > 0),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
            tables.counters
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_{}_stream_seq ON {} (stream_id, protocol, protocol_version, protocol_replay_sequence)",
            tables.log, tables.log
        ),
        format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_{}_stream_seq_unique ON {} (stream_id, protocol, protocol_version, protocol_replay_sequence)",
            tables.log, tables.log
        ),
        format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_{}_stream_cursor_unique ON {} (stream_id, protocol, protocol_version, protocol_replay_cursor)",
            tables.log, tables.log
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_{}_wire_id ON {} (protocol, protocol_version, wire_event_id)",
            tables.log, tables.log
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

#[async_trait]
impl ProtocolReplayWriter for PostgresStore {
    async fn append_replay(
        &self,
        draft: ProtocolReplayDraft,
    ) -> Result<ProtocolReplayAppendResult, ProtocolReplayError> {
        self.ensure_schema()
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
        draft.validate()?;
        let stream = ProtocolStreamKey::new(
            draft.stream_id.clone(),
            draft.protocol.clone(),
            draft.protocol_version.clone(),
        )?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
        if let Some(existing) = self.load_wire_event_tx(&mut tx, &stream, &draft).await? {
            if replay_record_matches_draft(&existing, &draft) {
                tx.commit()
                    .await
                    .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
                return Ok(ProtocolReplayAppendResult { record: existing });
            }
            return Err(ProtocolReplayError::Conflict(format!(
                "wire_event_id reused with different replay row: {}",
                draft.wire_event_id
            )));
        }

        let sequence = self.next_replay_sequence_tx(&mut tx, &stream).await?;
        let record = ProtocolReplayRecord::from_append(
            ProtocolReplayId::new(format!("pr_{}", uuid::Uuid::now_v7()))?,
            ProtocolReplayCursor::new(format!("prcur_pg_{sequence}"))?,
            current_millis_i64()? as u64,
            draft,
        )?;
        self.insert_replay_record_tx(&mut tx, &record, sequence)
            .await?;
        crate::postgres_outbox::insert_protocol_replay_outbox_tx(self, &mut tx, &record)
            .await
            .map_err(outbox_to_protocol_replay_error)?;
        tx.commit()
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
        Ok(ProtocolReplayAppendResult { record })
    }
}

#[async_trait]
impl ProtocolReplayReader for PostgresStore {
    async fn list_replay(
        &self,
        stream: ProtocolStreamKey,
        from: Option<ProtocolReplayCursor>,
        limit: usize,
    ) -> Result<ProtocolReplayPage, ProtocolReplayError> {
        self.ensure_schema()
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
        stream.validate()?;
        let now = current_millis_i64()?;
        let start_sequence = match from.as_ref() {
            Some(cursor) => Some(self.replay_cursor_sequence(&stream, cursor, now).await?),
            None => None,
        };
        let tables = self.protocol_replay_tables();
        let fetch_limit = limit.saturating_add(1);
        let sql = format!(
            "SELECT *
             FROM {}
             WHERE stream_id = $1 AND protocol = $2 AND protocol_version = $3
               AND protocol_replay_sequence > $4
               AND (expires_at IS NULL OR expires_at > $5)
             ORDER BY protocol_replay_sequence ASC
             LIMIT $6",
            tables.log
        );
        let rows = sqlx::query(&sql)
            .bind(&stream.stream_id)
            .bind(&stream.protocol)
            .bind(&stream.protocol_version)
            .bind(start_sequence.unwrap_or(0))
            .bind(now)
            .bind(fetch_limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
        let has_more = rows.len() > limit;
        let rows = if has_more { &rows[..limit] } else { &rows[..] };
        let records = rows
            .iter()
            .map(protocol_replay_record_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = if has_more {
            records
                .last()
                .map(|record| record.protocol_replay_cursor.clone())
        } else {
            None
        };
        Ok(ProtocolReplayPage {
            records,
            next_cursor,
            has_more,
        })
    }
}

#[async_trait]
impl ProtocolReplayLookup for PostgresStore {
    async fn load_replay(
        &self,
        protocol_replay_id: &ProtocolReplayId,
    ) -> Result<Option<ProtocolReplayRecord>, ProtocolReplayError> {
        self.ensure_schema()
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
        let tables = self.protocol_replay_tables();
        let sql = format!("SELECT * FROM {} WHERE protocol_replay_id = $1", tables.log);
        sqlx::query(&sql)
            .bind(protocol_replay_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?
            .map(|row| protocol_replay_record_from_row(&row))
            .transpose()
    }
}

impl PostgresStore {
    fn protocol_replay_tables(&self) -> ProtocolReplayTables {
        ProtocolReplayTables::from_store(self)
    }

    async fn load_wire_event_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        _stream: &ProtocolStreamKey,
        draft: &ProtocolReplayDraft,
    ) -> Result<Option<ProtocolReplayRecord>, ProtocolReplayError> {
        let tables = self.protocol_replay_tables();
        let sql = format!(
            "SELECT *
             FROM {}
             WHERE protocol = $1 AND protocol_version = $2
               AND wire_event_id = $3",
            tables.log
        );
        sqlx::query(&sql)
            .bind(&draft.protocol)
            .bind(&draft.protocol_version)
            .bind(&draft.wire_event_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?
            .map(|row| protocol_replay_record_from_row(&row))
            .transpose()
    }

    async fn next_replay_sequence_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        stream: &ProtocolStreamKey,
    ) -> Result<i64, ProtocolReplayError> {
        let tables = self.protocol_replay_tables();
        let scope_json = serde_json::json!({ "stream_id": stream.stream_id });
        let sql = format!(
            "INSERT INTO {} (stream_key, scope_key, scope_json, protocol, protocol_version, last_sequence)
             VALUES ($1, $2, $3, $4, $5, 1)
             ON CONFLICT (stream_key) DO UPDATE SET
                 last_sequence = {}.last_sequence + 1,
                 updated_at = now()
             RETURNING last_sequence",
            tables.counters, tables.counters
        );
        let row = sqlx::query(&sql)
            .bind(stream_key(stream)?)
            .bind(&stream.stream_id)
            .bind(scope_json)
            .bind(&stream.protocol)
            .bind(&stream.protocol_version)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
        row.try_get("last_sequence")
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))
    }

    async fn insert_replay_record_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        record: &ProtocolReplayRecord,
        sequence: i64,
    ) -> Result<(), ProtocolReplayError> {
        let tables = self.protocol_replay_tables();
        let scope_json = serde_json::json!({ "stream_id": record.stream_id });
        let sql = format!(
            "INSERT INTO {} (
                protocol_replay_id, stream_id, scope_key, scope_json, scope_type, scope_id,
                protocol, protocol_version,
                projector_version, protocol_replay_sequence, protocol_replay_cursor,
                wire_event_id, wire_event_type, wire_payload_bytes, wire_payload_json,
                source_event_ids, source_event_cursors, redaction_state, created_at,
                expires_at
            )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                     $13, $14, $15, $16, $17, $18, $19, $20)",
            tables.log
        );
        sqlx::query(&sql)
            .bind(record.protocol_replay_id.as_str())
            .bind(&record.stream_id)
            .bind(&record.stream_id)
            .bind(scope_json)
            .bind("protocol_stream")
            .bind(&record.stream_id)
            .bind(&record.protocol)
            .bind(&record.protocol_version)
            .bind(&record.projector_version)
            .bind(sequence)
            .bind(record.protocol_replay_cursor.as_str())
            .bind(&record.wire_event_id)
            .bind(&record.wire_event_type)
            .bind(&record.wire_payload_bytes)
            .bind(&record.wire_payload_json)
            .bind(json_value(&record.source_event_ids)?)
            .bind(json_value(&record.source_event_cursors)?)
            .bind(redaction_state_as_str(record.redaction_state))
            .bind(record.created_at as i64)
            .bind(record.expires_at.map(|value| value as i64))
            .execute(&mut **tx)
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
        Ok(())
    }

    async fn replay_cursor_sequence(
        &self,
        stream: &ProtocolStreamKey,
        cursor: &ProtocolReplayCursor,
        now: i64,
    ) -> Result<i64, ProtocolReplayError> {
        let tables = self.protocol_replay_tables();
        let sql = format!(
            "SELECT protocol_replay_sequence, expires_at
             FROM {}
             WHERE stream_id = $1 AND protocol = $2 AND protocol_version = $3
               AND protocol_replay_cursor = $4",
            tables.log
        );
        let row = sqlx::query(&sql)
            .bind(&stream.stream_id)
            .bind(&stream.protocol)
            .bind(&stream.protocol_version)
            .bind(cursor.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
        if let Some(row) = row {
            let expires_at = row
                .try_get::<Option<i64>, _>("expires_at")
                .map_err(|error| ProtocolReplayError::Io(error.to_string()))?;
            if expires_at.is_some_and(|expires_at| expires_at <= now) {
                return Err(ProtocolReplayError::CursorExpired(
                    cursor.as_str().to_string(),
                ));
            }
            return row
                .try_get("protocol_replay_sequence")
                .map_err(|error| ProtocolReplayError::Io(error.to_string()));
        }

        // ADR-0034 D8: a row missing inside retention must surface as an
        // Integrity error (operators alert), only true expiry returns
        // CursorExpired. Resolve "inside retention" by looking up the
        // counters row, which tracks the last cursor issued for this
        // stream — its bookkeeping is independent of cursor format and
        // therefore works even if the cursor was minted by another
        // store implementation or a future projector revision.
        let last_sequence = self.stream_last_sequence(stream).await?;
        if let Some(sequence) = postgres_replay_cursor_sequence(cursor) {
            if last_sequence.is_some_and(|last| sequence <= last) {
                return Err(ProtocolReplayError::Integrity(format!(
                    "protocol replay cursor points at missing row inside retained stream: {}",
                    cursor.as_str()
                )));
            }
        } else if last_sequence.is_some() {
            // The stream has retained history but we cannot place this
            // cursor relative to it. Treat as integrity (alertable) rather
            // than silently expiring — a foreign-format cursor reaching
            // this backend on a known stream is a bookkeeping bug, not a
            // benign retention boundary.
            return Err(ProtocolReplayError::Integrity(format!(
                "protocol replay cursor format not recognized by postgres backend \
                 but stream has retained history: {}",
                cursor.as_str()
            )));
        }
        Err(ProtocolReplayError::CursorExpired(
            cursor.as_str().to_string(),
        ))
    }

    async fn stream_last_sequence(
        &self,
        stream: &ProtocolStreamKey,
    ) -> Result<Option<i64>, ProtocolReplayError> {
        let tables = self.protocol_replay_tables();
        let sql = format!(
            "SELECT last_sequence
             FROM {}
             WHERE stream_key = $1",
            tables.counters
        );
        sqlx::query(&sql)
            .bind(stream_key(stream)?)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))?
            .map(|row| row.try_get("last_sequence"))
            .transpose()
            .map_err(|error| ProtocolReplayError::Io(error.to_string()))
    }
}

fn protocol_replay_record_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ProtocolReplayRecord, ProtocolReplayError> {
    macro_rules! get_t {
        ($ty:ty, $name:literal) => {
            row.try_get::<$ty, _>($name)
                .map_err(|error| ProtocolReplayError::Io(error.to_string()))?
        };
    }

    let stream_id = match get_t!(Option<String>, "stream_id") {
        Some(value) => value,
        None => get_t!(String, "scope_key"),
    };
    Ok(ProtocolReplayRecord {
        protocol_replay_id: ProtocolReplayId::new(get_t!(String, "protocol_replay_id"))?,
        stream_id,
        protocol: get_t!(String, "protocol"),
        protocol_version: get_t!(String, "protocol_version"),
        projector_version: get_t!(String, "projector_version"),
        wire_event_id: get_t!(String, "wire_event_id"),
        wire_event_type: get_t!(String, "wire_event_type"),
        wire_payload_bytes: get_t!(Vec<u8>, "wire_payload_bytes"),
        wire_payload_json: get_t!(Option<serde_json::Value>, "wire_payload_json"),
        source_event_ids: from_json(get_t!(serde_json::Value, "source_event_ids"))?,
        source_event_cursors: from_json(get_t!(serde_json::Value, "source_event_cursors"))?,
        protocol_replay_cursor: ProtocolReplayCursor::new(get_t!(
            String,
            "protocol_replay_cursor"
        ))?,
        redaction_state: parse_redaction_state(get_t!(String, "redaction_state"))?,
        expires_at: get_t!(Option<i64>, "expires_at").map(|value| value as u64),
        created_at: get_t!(i64, "created_at") as u64,
    })
}

fn replay_record_matches_draft(record: &ProtocolReplayRecord, draft: &ProtocolReplayDraft) -> bool {
    record.stream_id == draft.stream_id
        && record.protocol == draft.protocol
        && record.protocol_version == draft.protocol_version
        && record.projector_version == draft.projector_version
        && record.wire_event_id == draft.wire_event_id
        && record.wire_event_type == draft.wire_event_type
        && record.wire_payload_bytes == draft.wire_payload_bytes
        && record.wire_payload_json == draft.wire_payload_json
        && record.source_event_ids == draft.source_event_ids
        && record.source_event_cursors == draft.source_event_cursors
        && record.redaction_state == draft.redaction_state
        && record.expires_at == draft.expires_at
}

fn stream_key(stream: &ProtocolStreamKey) -> Result<String, ProtocolReplayError> {
    Ok(format!(
        "{}|{}|{}",
        stream.stream_id, stream.protocol, stream.protocol_version
    ))
}

fn postgres_replay_cursor_sequence(cursor: &ProtocolReplayCursor) -> Option<i64> {
    cursor
        .as_str()
        .strip_prefix("prcur_pg_")?
        .parse::<i64>()
        .ok()
        .filter(|sequence| *sequence > 0)
}

fn json_value<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, ProtocolReplayError> {
    serde_json::to_value(value)
        .map_err(|error| ProtocolReplayError::Serialization(error.to_string()))
}

fn from_json<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, ProtocolReplayError> {
    serde_json::from_value(value)
        .map_err(|error| ProtocolReplayError::Serialization(error.to_string()))
}

fn redaction_state_as_str(state: ProtocolReplayRedactionState) -> &'static str {
    match state {
        ProtocolReplayRedactionState::Clear => "clear",
        ProtocolReplayRedactionState::Redacted => "redacted",
    }
}

fn parse_redaction_state(
    value: String,
) -> Result<ProtocolReplayRedactionState, ProtocolReplayError> {
    match value.as_str() {
        "clear" => Ok(ProtocolReplayRedactionState::Clear),
        "redacted" => Ok(ProtocolReplayRedactionState::Redacted),
        other => Err(ProtocolReplayError::Integrity(format!(
            "unknown protocol replay redaction state: {other}"
        ))),
    }
}

fn current_millis_i64() -> Result<i64, ProtocolReplayError> {
    i64::try_from(crate::current_millis())
        .map_err(|error| ProtocolReplayError::Io(error.to_string()))
}

fn outbox_to_protocol_replay_error(error: OutboxError) -> ProtocolReplayError {
    match error {
        OutboxError::Validation(message) => ProtocolReplayError::Validation(message),
        OutboxError::Conflict(message) => ProtocolReplayError::Integrity(message),
        OutboxError::Io(message) => ProtocolReplayError::Io(message),
        OutboxError::Serialization(message) => ProtocolReplayError::Serialization(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_key_distinguishes_protocol_versions() {
        let v6 = ProtocolStreamKey::new("thread:t1", "ai-sdk", "v6").unwrap();
        let v7 = ProtocolStreamKey::new("thread:t1", "ai-sdk", "v7").unwrap();
        assert_ne!(stream_key(&v6).unwrap(), stream_key(&v7).unwrap());
    }

    #[test]
    fn redaction_state_round_trips_wire_values() {
        for state in [
            ProtocolReplayRedactionState::Clear,
            ProtocolReplayRedactionState::Redacted,
        ] {
            assert_eq!(
                parse_redaction_state(redaction_state_as_str(state).to_string()).unwrap(),
                state
            );
        }
    }

    #[test]
    fn postgres_replay_cursor_sequence_accepts_only_store_generated_cursors() {
        assert_eq!(
            postgres_replay_cursor_sequence(&ProtocolReplayCursor::new("prcur_pg_42").unwrap()),
            Some(42)
        );
        assert_eq!(
            postgres_replay_cursor_sequence(&ProtocolReplayCursor::new("prcur_pg_0").unwrap()),
            None
        );
        assert_eq!(
            postgres_replay_cursor_sequence(&ProtocolReplayCursor::new("prcur_mem_1").unwrap()),
            None
        );
        assert_eq!(
            postgres_replay_cursor_sequence(&ProtocolReplayCursor::new("prcur_pg_nope").unwrap()),
            None
        );
    }
}
