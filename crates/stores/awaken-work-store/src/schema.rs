//! Portable work-queue schema, row codec, and lease-time normalization shared
//! by the SQLite and PostgreSQL adapters.

use std::collections::BTreeMap;

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use awaken_session_contract::work_queue::{
    WorkItem, WorkPayload, WorkState, next_heartbeat_receipt,
};

/// Frozen presence timestamp used by the open-tier Managed projection.
pub(super) const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
pub(super) const HEARTBEAT_TTL_SECONDS: u64 = 60;
pub(super) const NS: &str = "work_queue";

pub(super) fn work_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.work_queue",
        vec![Migration::new(
            1,
            "current self-hosted work queue authority",
            "CREATE TABLE {prefix}_item (\
             work_id             TEXT PRIMARY KEY, \
             seq                 BIGINT NOT NULL CHECK (seq >= 0), \
             environment_id      TEXT NOT NULL CHECK (length(environment_id) > 0), \
             data_type           TEXT NOT NULL CHECK (data_type IN ('healthcheck', 'session')), \
             data_id             TEXT NOT NULL CHECK (length(data_id) > 0), \
             metadata_json       TEXT NOT NULL, \
             state               TEXT NOT NULL CHECK (state IN ('queued', 'starting', 'active', 'stopping', 'stopped')), \
             acknowledged_at     TEXT, \
             latest_heartbeat_at TEXT, \
             started_at          TEXT, \
             stop_requested_at   TEXT, \
             stopped_at          TEXT, \
             lease_owner         TEXT, \
             lease_epoch         BIGINT NOT NULL DEFAULT 0 CHECK (lease_epoch >= 0), \
             lease_expires_ms    BIGINT CHECK (lease_expires_ms IS NULL OR lease_expires_ms >= 0), \
             lease_refreshed_ms  BIGINT CHECK (lease_refreshed_ms IS NULL OR lease_refreshed_ms >= 0), \
             session_token_sha256 TEXT); \
             CREATE UNIQUE INDEX {prefix}_session_projection_unique \
             ON {prefix}_item (environment_id, data_type, data_id)",
        )?],
    )
}

fn state_from_wire(state: &str) -> Result<WorkState, String> {
    WorkState::from_wire(state).ok_or_else(|| format!("unknown persisted work state `{state}`"))
}

fn data_of(data_type: &str, data_id: String) -> Result<WorkPayload, String> {
    match data_type {
        "healthcheck" => Ok(WorkPayload::HealthCheck { id: data_id }),
        "session" => Ok(WorkPayload::Session { id: data_id }),
        _ => Err(format!("unknown persisted work payload type `{data_type}`")),
    }
}

pub(super) const COLS: &str = "work_id, environment_id, data_type, data_id, metadata_json, state, \
     acknowledged_at, latest_heartbeat_at, started_at, stop_requested_at, stopped_at";

pub(super) fn metadata_str(metadata: &BTreeMap<String, String>) -> String {
    serde_json::to_string(metadata).expect("work metadata serializes")
}

pub(super) fn db_millis(now_ms: u64) -> i64 {
    i64::try_from(now_ms).unwrap_or(i64::MAX)
}

pub(super) fn effective_ttl_seconds(desired: Option<u64>) -> u64 {
    desired.unwrap_or(HEARTBEAT_TTL_SECONDS).max(1)
}

pub(super) fn lease_expiry(now_ms: u64, ttl_seconds: u64) -> i64 {
    db_millis(now_ms.saturating_add(ttl_seconds.saturating_mul(1000)))
}

/// Produce the monotonic RFC-3339 compare token returned by the Managed API.
pub(crate) fn heartbeat_at(now_ms: u64, previous: Option<&str>) -> String {
    next_heartbeat_receipt(now_ms, previous)
}

pub(super) fn ack_next_state(current: &WorkItem) -> &'static str {
    current.state.after_ack().as_str()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_item(
    id: String,
    environment_id: String,
    data_type: &str,
    data_id: String,
    metadata_json: &str,
    state: &str,
    acknowledged_at: Option<String>,
    latest_heartbeat_at: Option<String>,
    started_at: Option<String>,
    stop_requested_at: Option<String>,
    stopped_at: Option<String>,
) -> Result<WorkItem, String> {
    let metadata = serde_json::from_str(metadata_json)
        .map_err(|error| format!("invalid persisted work metadata: {error}"))?;
    Ok(WorkItem {
        id,
        environment_id,
        data: data_of(data_type, data_id)?,
        metadata,
        state: state_from_wire(state)?,
        acknowledged_at,
        latest_heartbeat_at,
        started_at,
        stop_requested_at,
        stopped_at,
    })
}

pub(super) fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItem> {
    let metadata_json: String = row.get(4)?;
    build_item(
        row.get(0)?,
        row.get(1)?,
        &row.get::<_, String>(2)?,
        row.get(3)?,
        &metadata_json,
        &row.get::<_, String>(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    )
    .map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
        )
    })
}
