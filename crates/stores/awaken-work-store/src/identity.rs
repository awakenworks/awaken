// @formal surface obligation=work.lease_authority_snapshot_and_owner_mutations_are_atomic
// @formal reason: A durable generation and transactional high-water mark keep retired Work ids
// @formal reason: from authorizing operations on replacement rows after deletion or restart.
//! Physical-store Work identity. Ordering rows are never an identity allocator.

use awaken_executable_environment_contract::WorkQueueError;
use awaken_sqlite_pool::EmbeddedPool;
use sqlx::{PgConnection, PgPool, SqliteConnection};

use crate::storage;

const ALLOCATE: &str = "UPDATE work_queue_identity_allocator \
    SET next_sequence = (CASE WHEN next_sequence < $1 THEN $1 ELSE next_sequence END) + 1 \
    WHERE singleton = 1 AND next_sequence < 9223372036854775807 AND $1 < 9223372036854775807 \
    RETURNING generation_id, next_sequence - 1";
const READ: &str = "SELECT generation_id, next_sequence FROM work_queue_identity_allocator";

fn validate(generation: &str, sequence: i64) -> Result<(), String> {
    if generation.len() != 32
        || !generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || sequence < 0
    {
        return Err("invalid durable Work identity allocator".into());
    }
    Ok(())
}

fn allocated(row: Option<(String, i64)>) -> Result<(i64, String), WorkQueueError> {
    let (generation, sequence) =
        row.ok_or_else(|| storage("durable Work identity allocator is missing or exhausted"))?;
    validate(&generation, sequence).map_err(storage)?;
    // Legacy numeric ids are kept unchanged. The new namespace cannot alias
    // even a deleted legacy id that was never recorded in the remaining rows.
    Ok((sequence, format!("work_g{generation}{sequence:016x}")))
}

fn verify(rows: Vec<(String, i64)>) -> Result<(), String> {
    if rows.len() != 1 {
        return Err("durable Work identity allocator must have exactly one row".into());
    }
    validate(&rows[0].0, rows[0].1)
}

pub(super) async fn allocate_sqlite(
    connection: &mut SqliteConnection,
) -> Result<(i64, String), WorkQueueError> {
    // Surviving rows provide an ordering floor, never identity history. The
    // durable counter can only advance, even when every Work row is deleted.
    let last: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM work_queue_item")
        .fetch_one(&mut *connection)
        .await
        .map_err(storage)?;
    let minimum = match last {
        None => 0,
        Some(sequence) => sequence
            .checked_add(1)
            .ok_or_else(|| storage("Work ordering space exhausted"))?,
    };
    allocated(
        sqlx::query_as(ALLOCATE)
            .bind(minimum)
            .fetch_optional(connection)
            .await
            .map_err(storage)?,
    )
}

pub(super) async fn allocate_postgres(
    connection: &mut PgConnection,
) -> Result<(i64, String), WorkQueueError> {
    // Surviving rows provide an ordering floor, never identity history. The
    // durable counter can only advance, even when every Work row is deleted.
    let last: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM work_queue_item")
        .fetch_one(&mut *connection)
        .await
        .map_err(storage)?;
    let minimum = match last {
        None => 0,
        Some(sequence) => sequence
            .checked_add(1)
            .ok_or_else(|| storage("Work ordering space exhausted"))?,
    };
    allocated(
        sqlx::query_as(ALLOCATE)
            .bind(minimum)
            .fetch_optional(connection)
            .await
            .map_err(storage)?,
    )
}

// Verification never repairs the allocator from MAX(seq). Missing authority
// after a recorded migration is corruption, not permission to reuse identities.
pub(super) async fn verify_sqlite(pool: &EmbeddedPool) -> Result<(), String> {
    verify(
        sqlx::query_as(READ)
            .fetch_all(pool)
            .await
            .map_err(|error| error.to_string())?,
    )
}

pub(super) async fn verify_postgres(pool: &PgPool) -> Result<(), String> {
    verify(
        sqlx::query_as(READ)
            .fetch_all(pool)
            .await
            .map_err(|error| error.to_string())?,
    )
}
