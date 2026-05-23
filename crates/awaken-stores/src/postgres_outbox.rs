//! PostgreSQL outbox-store implementation.

use async_trait::async_trait;
use awaken_contract::contract::event_store::{CanonicalEvent, EventStoreError};
use awaken_contract::contract::outbox::{
    OUTBOX_LANE_CANONICAL, OUTBOX_LANE_PROTOCOL_REPLAY, OUTBOX_TARGET_PROTOCOL_FANOUT,
    OUTBOX_TARGET_PROTOCOL_PROJECTOR, OutboxEnqueueResult, OutboxError, OutboxMessage,
    OutboxMessageDraft, OutboxNackOutcome, OutboxStatus, OutboxStore,
};
use awaken_contract::contract::protocol_replay_log::ProtocolReplayRecord;
use awaken_contract::contract::storage::StorageError;
use sqlx::{Postgres, Row, Transaction};

use crate::postgres::PostgresStore;

struct OutboxTables {
    messages: String,
}

impl OutboxTables {
    fn from_store(store: &PostgresStore) -> Self {
        let prefix = store
            .threads_table
            .strip_suffix("_threads")
            .unwrap_or(&store.threads_table);
        Self {
            messages: format!("{prefix}_outbox"),
        }
    }
}

pub(crate) async fn ensure_outbox_schema(store: &PostgresStore) -> Result<(), StorageError> {
    let tables = OutboxTables::from_store(store);
    let statements = vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                outbox_id TEXT PRIMARY KEY,
                lane TEXT NOT NULL,
                target TEXT NOT NULL,
                payload JSONB NOT NULL,
                dedupe_key TEXT,
                append_digest BYTEA,
                status TEXT NOT NULL,
                available_at BIGINT NOT NULL,
                attempt_count INTEGER NOT NULL,
                max_attempts INTEGER NOT NULL CHECK (max_attempts > 0),
                claimed_by TEXT,
                claim_token TEXT,
                lease_expires_at BIGINT,
                last_error TEXT,
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL
            )",
            tables.messages
        ),
        format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_{}_dedupe
             ON {} (lane, target, dedupe_key)
             WHERE dedupe_key IS NOT NULL",
            tables.messages, tables.messages
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_{}_claimable
             ON {} (lane, target, status, available_at, created_at)",
            tables.messages, tables.messages
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

/// Attach an outbox insert to an externally-managed Postgres transaction.
/// Use this when a control-plane writer needs `BEGIN ... domain write ...
/// EventStore append ... outbox insert ... COMMIT` atomicity (ADR-0034 D9).
/// Caller is responsible for opening, committing, and rolling back the
/// transaction; only the outbox insert is performed here.
pub async fn enqueue_outbox_in_transaction(
    store: &PostgresStore,
    tx: &mut Transaction<'_, Postgres>,
    draft: OutboxMessageDraft,
) -> Result<OutboxEnqueueResult, OutboxError> {
    draft.validate()?;
    let digest = draft_digest(&draft)?;
    if let Some(existing) = store.load_dedupe_tx(&mut *tx, &draft).await? {
        return Ok(OutboxEnqueueResult { message: existing });
    }
    let outbox_id = format!("out_{}", uuid::Uuid::now_v7());
    let message = OutboxMessage::from_enqueue(outbox_id, draft, crate::current_millis())?;
    store.insert_outbox_tx(&mut *tx, &message, &digest).await?;
    Ok(OutboxEnqueueResult { message })
}

pub(crate) async fn insert_canonical_event_outbox_tx(
    store: &PostgresStore,
    tx: &mut Transaction<'_, Postgres>,
    event: &CanonicalEvent,
    created_at: i64,
) -> Result<(), EventStoreError> {
    let mut draft = OutboxMessageDraft::new(
        OUTBOX_LANE_CANONICAL,
        OUTBOX_TARGET_PROTOCOL_PROJECTOR,
        serde_json::json!({
            "event_id": event.event_id.as_str(),
            "event_kind": event.event_kind.as_str(),
            "created_at": event.created_at,
        }),
    )
    .map_err(outbox_to_event_error)?;
    draft.dedupe_key = Some(format!("canonical/{}", event.event_id.as_str()));
    let digest = draft_digest(&draft).map_err(outbox_to_event_error)?;
    let message = OutboxMessage::from_enqueue(
        format!("out_{}", uuid::Uuid::now_v7()),
        draft,
        created_at as u64,
    )
    .map_err(outbox_to_event_error)?;
    store
        .insert_outbox_tx(tx, &message, &digest)
        .await
        .map_err(outbox_to_event_error)
}

pub(crate) async fn insert_protocol_replay_outbox_tx(
    store: &PostgresStore,
    tx: &mut Transaction<'_, Postgres>,
    record: &ProtocolReplayRecord,
) -> Result<(), OutboxError> {
    let mut draft = OutboxMessageDraft::new(
        OUTBOX_LANE_PROTOCOL_REPLAY,
        OUTBOX_TARGET_PROTOCOL_FANOUT,
        serde_json::json!({
            "protocol_replay_id": record.protocol_replay_id.as_str(),
            "protocol": record.protocol.as_str(),
            "protocol_version": record.protocol_version.as_str(),
            "wire_event_id": record.wire_event_id.as_str(),
        }),
    )?;
    draft.dedupe_key = Some(format!(
        "protocol_replay/{}",
        record.protocol_replay_id.as_str()
    ));
    let digest = draft_digest(&draft)?;
    let message = OutboxMessage::from_enqueue(
        format!("out_{}", uuid::Uuid::now_v7()),
        draft,
        record.created_at,
    )?;
    store.insert_outbox_tx(tx, &message, &digest).await
}

#[async_trait]
impl OutboxStore for PostgresStore {
    async fn enqueue_outbox(
        &self,
        draft: OutboxMessageDraft,
    ) -> Result<OutboxEnqueueResult, OutboxError> {
        self.ensure_schema()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        draft.validate()?;
        let digest = draft_digest(&draft)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        if let Some(existing) = self.load_dedupe_tx(&mut tx, &draft).await? {
            let append_digest = existing_append_digest(&mut tx, self, &existing.outbox_id).await?;
            if append_digest.as_deref() == Some(digest.as_slice()) {
                tx.commit()
                    .await
                    .map_err(|error| OutboxError::Io(error.to_string()))?;
                return Ok(OutboxEnqueueResult { message: existing });
            }
            return Err(OutboxError::Conflict(format!(
                "outbox dedupe key reused with different input: {}",
                draft.dedupe_key.as_deref().unwrap_or("")
            )));
        }

        let now = current_millis_i64()?;
        let message = OutboxMessage::from_enqueue(
            format!("out_{}", uuid::Uuid::now_v7()),
            draft,
            now as u64,
        )?;
        self.insert_outbox_tx(&mut tx, &message, &digest).await?;
        tx.commit()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        Ok(OutboxEnqueueResult { message })
    }

    async fn claim_outbox(
        &self,
        lane: &str,
        target: &str,
        limit: usize,
        lease_ms: u64,
        consumer_id: &str,
        now: u64,
    ) -> Result<Vec<OutboxMessage>, OutboxError> {
        reject_blank("lane", lane)?;
        reject_blank("target", target)?;
        reject_blank("consumer_id", consumer_id)?;
        self.ensure_schema()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        let ids = self
            .claimable_ids_tx(&mut tx, lane, target, limit, now as i64)
            .await?;
        let mut claimed = Vec::with_capacity(ids.len());
        for outbox_id in ids {
            let claim_token = format!("claim_{}", uuid::Uuid::now_v7());
            let sql = format!(
                "UPDATE {}
                 SET status = 'claimed',
                     attempt_count = attempt_count + 1,
                     claimed_by = $2,
                     claim_token = $3,
                     lease_expires_at = $4,
                     updated_at = $5
                 WHERE outbox_id = $1
                 RETURNING *",
                self.outbox_tables().messages
            );
            let row = sqlx::query(&sql)
                .bind(&outbox_id)
                .bind(consumer_id)
                .bind(claim_token)
                .bind(now.saturating_add(lease_ms) as i64)
                .bind(now as i64)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| OutboxError::Io(error.to_string()))?;
            claimed.push(outbox_message_from_row(&row)?);
        }
        tx.commit()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        Ok(claimed)
    }

    async fn ack_outbox(
        &self,
        outbox_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<bool, OutboxError> {
        self.ensure_schema()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        let sql = format!(
            "UPDATE {}
             SET status = 'delivered',
                 claimed_by = NULL,
                 claim_token = NULL,
                 lease_expires_at = NULL,
                 updated_at = $3
             WHERE outbox_id = $1 AND status = 'claimed' AND claim_token = $2",
            self.outbox_tables().messages
        );
        let result = sqlx::query(&sql)
            .bind(outbox_id)
            .bind(claim_token)
            .bind(now as i64)
            .execute(&self.pool)
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        Ok(result.rows_affected() == 1)
    }

    async fn nack_outbox(
        &self,
        outbox_id: &str,
        claim_token: &str,
        error: &str,
        retry_at: u64,
        now: u64,
    ) -> Result<OutboxNackOutcome, OutboxError> {
        self.ensure_schema()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        let Some(message) = self.load_outbox_for_update_tx(&mut tx, outbox_id).await? else {
            return Ok(OutboxNackOutcome::LostClaim);
        };
        if message.status != OutboxStatus::Claimed
            || message.claim_token.as_deref() != Some(claim_token)
        {
            return Ok(OutboxNackOutcome::LostClaim);
        }
        let dead_letter = message.attempt_count >= message.max_attempts;
        let status = if dead_letter {
            "dead_letter"
        } else {
            "pending"
        };
        let available_at = if dead_letter {
            message.available_at
        } else {
            retry_at
        };
        let sql = format!(
            "UPDATE {}
             SET status = $2,
                 available_at = $3,
                 claimed_by = NULL,
                 claim_token = NULL,
                 lease_expires_at = NULL,
                 last_error = $4,
                 updated_at = $5
             WHERE outbox_id = $1",
            self.outbox_tables().messages
        );
        sqlx::query(&sql)
            .bind(outbox_id)
            .bind(status)
            .bind(available_at as i64)
            .bind(error)
            .bind(now as i64)
            .execute(&mut *tx)
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        Ok(if dead_letter {
            OutboxNackOutcome::DeadLettered
        } else {
            OutboxNackOutcome::Requeued
        })
    }

    async fn list_outbox(
        &self,
        status: Option<OutboxStatus>,
        limit: usize,
    ) -> Result<Vec<OutboxMessage>, OutboxError> {
        self.ensure_schema()
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        let tables = self.outbox_tables();
        let (sql, status_string) = if let Some(status) = status {
            (
                format!(
                    "SELECT * FROM {} WHERE status = $1 ORDER BY created_at ASC, outbox_id ASC LIMIT $2",
                    tables.messages
                ),
                Some(status_to_str(status).to_string()),
            )
        } else {
            (
                format!(
                    "SELECT * FROM {} ORDER BY created_at ASC, outbox_id ASC LIMIT $1",
                    tables.messages
                ),
                None,
            )
        };
        let rows = if let Some(status) = status_string {
            sqlx::query(&sql)
                .bind(status)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await
        } else {
            sqlx::query(&sql)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await
        }
        .map_err(|error| OutboxError::Io(error.to_string()))?;
        rows.iter().map(outbox_message_from_row).collect()
    }
}

impl PostgresStore {
    fn outbox_tables(&self) -> OutboxTables {
        OutboxTables::from_store(self)
    }

    async fn load_dedupe_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        draft: &OutboxMessageDraft,
    ) -> Result<Option<OutboxMessage>, OutboxError> {
        let Some(dedupe_key) = draft.dedupe_key.as_ref() else {
            return Ok(None);
        };
        let sql = format!(
            "SELECT * FROM {}
             WHERE lane = $1 AND target = $2 AND dedupe_key = $3
             FOR UPDATE",
            self.outbox_tables().messages
        );
        sqlx::query(&sql)
            .bind(&draft.lane)
            .bind(&draft.target)
            .bind(dedupe_key)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?
            .map(|row| outbox_message_from_row(&row))
            .transpose()
    }

    async fn insert_outbox_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        message: &OutboxMessage,
        digest: &[u8],
    ) -> Result<(), OutboxError> {
        let sql = format!(
            "INSERT INTO {} (
                outbox_id, lane, target, payload, dedupe_key, append_digest,
                status, available_at, attempt_count, max_attempts, claimed_by,
                claim_token, lease_expires_at, last_error, created_at, updated_at
            )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                     $13, $14, $15, $16)",
            self.outbox_tables().messages
        );
        sqlx::query(&sql)
            .bind(&message.outbox_id)
            .bind(&message.lane)
            .bind(&message.target)
            .bind(&message.payload)
            .bind(&message.dedupe_key)
            .bind(digest)
            .bind(status_to_str(message.status))
            .bind(message.available_at as i64)
            .bind(message.attempt_count as i32)
            .bind(message.max_attempts as i32)
            .bind(&message.claimed_by)
            .bind(&message.claim_token)
            .bind(message.lease_expires_at.map(|value| value as i64))
            .bind(&message.last_error)
            .bind(message.created_at as i64)
            .bind(message.updated_at as i64)
            .execute(&mut **tx)
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        Ok(())
    }

    async fn claimable_ids_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        lane: &str,
        target: &str,
        limit: usize,
        now: i64,
    ) -> Result<Vec<String>, OutboxError> {
        let sql = format!(
            "SELECT outbox_id FROM {}
             WHERE lane = $1 AND target = $2
               AND (
                 (status = 'pending' AND available_at <= $3)
                 OR (status = 'claimed' AND lease_expires_at <= $3)
               )
             ORDER BY available_at ASC, created_at ASC, outbox_id ASC
             LIMIT $4
             FOR UPDATE SKIP LOCKED",
            self.outbox_tables().messages
        );
        let rows = sqlx::query(&sql)
            .bind(lane)
            .bind(target)
            .bind(now)
            .bind(limit as i64)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?;
        rows.into_iter()
            .map(|row| {
                row.try_get("outbox_id")
                    .map_err(|error| OutboxError::Io(error.to_string()))
            })
            .collect()
    }

    async fn load_outbox_for_update_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        outbox_id: &str,
    ) -> Result<Option<OutboxMessage>, OutboxError> {
        let sql = format!(
            "SELECT * FROM {} WHERE outbox_id = $1 FOR UPDATE",
            self.outbox_tables().messages
        );
        sqlx::query(&sql)
            .bind(outbox_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| OutboxError::Io(error.to_string()))?
            .map(|row| outbox_message_from_row(&row))
            .transpose()
    }
}

async fn existing_append_digest(
    tx: &mut Transaction<'_, Postgres>,
    store: &PostgresStore,
    outbox_id: &str,
) -> Result<Option<Vec<u8>>, OutboxError> {
    let sql = format!(
        "SELECT append_digest FROM {} WHERE outbox_id = $1",
        store.outbox_tables().messages
    );
    let row = sqlx::query(&sql)
        .bind(outbox_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| OutboxError::Io(error.to_string()))?;
    row.try_get("append_digest")
        .map_err(|error| OutboxError::Io(error.to_string()))
}

fn outbox_message_from_row(row: &sqlx::postgres::PgRow) -> Result<OutboxMessage, OutboxError> {
    Ok(OutboxMessage {
        outbox_id: get(row, "outbox_id")?,
        lane: get(row, "lane")?,
        target: get(row, "target")?,
        payload: get(row, "payload")?,
        dedupe_key: get(row, "dedupe_key")?,
        status: status_from_str(get::<String>(row, "status")?.as_str())?,
        available_at: get::<i64>(row, "available_at")? as u64,
        attempt_count: get::<i32>(row, "attempt_count")? as u32,
        max_attempts: get::<i32>(row, "max_attempts")? as u32,
        claimed_by: get(row, "claimed_by")?,
        claim_token: get(row, "claim_token")?,
        lease_expires_at: get::<Option<i64>>(row, "lease_expires_at")?.map(|value| value as u64),
        last_error: get(row, "last_error")?,
        created_at: get::<i64>(row, "created_at")? as u64,
        updated_at: get::<i64>(row, "updated_at")? as u64,
    })
}

fn get<T>(row: &sqlx::postgres::PgRow, name: &str) -> Result<T, OutboxError>
where
    for<'r> T: sqlx::Decode<'r, Postgres> + sqlx::Type<Postgres>,
{
    row.try_get(name)
        .map_err(|error| OutboxError::Io(error.to_string()))
}

fn draft_digest(draft: &OutboxMessageDraft) -> Result<Vec<u8>, OutboxError> {
    serde_json::to_vec(draft).map_err(|error| OutboxError::Serialization(error.to_string()))
}

fn current_millis_i64() -> Result<i64, OutboxError> {
    i64::try_from(crate::current_millis())
        .map_err(|error| OutboxError::Validation(format!("current time out of range: {error}")))
}

fn status_to_str(status: OutboxStatus) -> &'static str {
    match status {
        OutboxStatus::Pending => "pending",
        OutboxStatus::Claimed => "claimed",
        OutboxStatus::Delivered => "delivered",
        OutboxStatus::DeadLetter => "dead_letter",
    }
}

fn status_from_str(value: &str) -> Result<OutboxStatus, OutboxError> {
    match value {
        "pending" => Ok(OutboxStatus::Pending),
        "claimed" => Ok(OutboxStatus::Claimed),
        "delivered" => Ok(OutboxStatus::Delivered),
        "dead_letter" => Ok(OutboxStatus::DeadLetter),
        other => Err(OutboxError::Io(format!(
            "unknown outbox status in database: {other}"
        ))),
    }
}

fn reject_blank(field: &str, value: &str) -> Result<(), OutboxError> {
    if value.trim().is_empty() {
        return Err(OutboxError::Validation(format!("{field} is required")));
    }
    Ok(())
}

fn outbox_to_event_error(error: OutboxError) -> EventStoreError {
    match error {
        OutboxError::Validation(message) => EventStoreError::Validation(message),
        OutboxError::Conflict(message) => EventStoreError::Integrity(message),
        OutboxError::Io(message) => EventStoreError::Io(message),
        OutboxError::Serialization(message) => EventStoreError::Serialization(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trip_matches_storage_values() {
        for status in [
            OutboxStatus::Pending,
            OutboxStatus::Claimed,
            OutboxStatus::Delivered,
            OutboxStatus::DeadLetter,
        ] {
            assert_eq!(status_from_str(status_to_str(status)).unwrap(), status);
        }
    }
}
