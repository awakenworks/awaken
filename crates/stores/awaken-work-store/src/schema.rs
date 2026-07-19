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
        vec![
            Migration::new(
                1,
                "self-hosted environment work queue: one row per work item",
                "CREATE TABLE {prefix}_item (\
             work_id             TEXT PRIMARY KEY, \
             seq                 BIGINT NOT NULL, \
             environment_id      TEXT NOT NULL, \
             data_type           TEXT NOT NULL, \
             data_id             TEXT NOT NULL, \
             metadata_json       TEXT NOT NULL, \
             state               TEXT NOT NULL, \
             acknowledged_at     TEXT, \
             latest_heartbeat_at TEXT, \
             started_at          TEXT, \
             stop_requested_at   TEXT, \
             stopped_at          TEXT)",
            )?,
            Migration::new(
                2,
                "persist work ownership, fencing epoch, and lease expiry",
                "ALTER TABLE {prefix}_item ADD COLUMN lease_owner TEXT; \
                 ALTER TABLE {prefix}_item ADD COLUMN lease_epoch BIGINT NOT NULL DEFAULT 0; \
                 ALTER TABLE {prefix}_item ADD COLUMN lease_expires_ms BIGINT",
            )?,
        ],
    )
}

fn state_from_wire(state: &str) -> WorkState {
    // Unknown persisted values fail closed instead of becoming claimable work.
    WorkState::from_wire(state).unwrap_or(WorkState::Stopped)
}

fn data_of(data_type: &str, data_id: String) -> WorkPayload {
    match data_type {
        "healthcheck" => WorkPayload::HealthCheck { id: data_id },
        _ => WorkPayload::Session { id: data_id },
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
) -> WorkItem {
    WorkItem {
        id,
        environment_id,
        data: data_of(data_type, data_id),
        metadata: serde_json::from_str(metadata_json).unwrap_or_default(),
        state: state_from_wire(state),
        acknowledged_at,
        latest_heartbeat_at,
        started_at,
        stop_requested_at,
        stopped_at,
    }
}

pub(super) fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItem> {
    let metadata_json: String = row.get(4)?;
    Ok(build_item(
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
    ))
}
