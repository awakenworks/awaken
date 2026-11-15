// @formal surface obligation=webhook.lifecycle_commit_and_outbox_are_atomic
// @formal reason: Replica delivery claims operate on facts from the original Session transaction.
//! PostgreSQL lifecycle outbox and replica delivery leases.

use super::{
    LifecycleOutbox, ManagedLifecycleFact, PgPool, PostgresManagedSessionRepository,
    SessionRepositoryError, corrupt, decode_lifecycle, lifecycle_str, storage,
};
use async_trait::async_trait;
use awaken_scoped_migration::postgres::PostgresMigrationRunner;
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use std::time::Duration;

fn delivery_bundle() -> Result<MigrationBundle, MigrationError> {
    // An independent additive bundle lets previous servers continue to
    // verify the original Session ledger and restart during a rollout.
    MigrationBundle::new(
        "awaken.managed_session.outbox_delivery",
        vec![Migration::new(
            1,
            "reserve lifecycle delivery across Coordinator replicas",
            "CREATE TABLE {prefix}_lifecycle_outbox_claim (
                 fact_id TEXT PRIMARY KEY,
                 token UUID NOT NULL,
                 lease_until {timestamptz} NOT NULL
             )",
        )?],
    )
}

async fn index_valid(connection: &mut sqlx::PgConnection) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT indisvalid FROM pg_index
         WHERE indexrelid = to_regclass('managed_lifecycle_outbox_delivery_idx')
           AND indrelid = 'managed_lifecycle_outbox'::regclass",
    )
    .fetch_optional(connection)
    .await
}

pub(super) async fn migrate(runner: &PostgresMigrationRunner, pool: &PgPool) -> Result<(), String> {
    runner
        .run_bundle(&delivery_bundle().map_err(|error| error.to_string())?)
        .await
        .map_err(|error| error.to_string())?;
    let mut connection = pool.acquire().await.map_err(|error| error.to_string())?;
    if index_valid(&mut connection)
        .await
        .map_err(|error| error.to_string())?
        == Some(true)
    {
        return Ok(());
    }

    // PostgreSQL requires CONCURRENTLY outside a transaction. Close this
    // dedicated checkout on every exit so cancellation also releases its lock.
    connection.close_on_drop();
    sqlx::query(
        "SELECT pg_advisory_lock(hashtext(current_schema()), hashtext('managed_lifecycle_outbox_delivery_idx'))",
    )
    .execute(&mut *connection)
    .await
    .map_err(|error| error.to_string())?;
    match index_valid(&mut connection)
        .await
        .map_err(|error| error.to_string())?
    {
        Some(true) => return Ok(()),
        Some(false) => {
            sqlx::query("DROP INDEX CONCURRENTLY managed_lifecycle_outbox_delivery_idx")
                .execute(&mut *connection)
                .await
                .map_err(|error| error.to_string())?;
        }
        None => {}
    }
    sqlx::query(
        "CREATE INDEX CONCURRENTLY managed_lifecycle_outbox_delivery_idx
         ON managed_lifecycle_outbox (created_at, fact_id)",
    )
    .execute(&mut *connection)
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub(super) async fn verify(runner: &PostgresMigrationRunner, pool: &PgPool) -> Result<(), String> {
    runner
        .verify_bundle(&delivery_bundle().map_err(|error| error.to_string())?)
        .await
        .map_err(|error| error.to_string())?;
    let mut connection = pool.acquire().await.map_err(|error| error.to_string())?;
    if index_valid(&mut connection)
        .await
        .map_err(|error| error.to_string())?
        != Some(true)
    {
        return Err("managed lifecycle outbox delivery index is missing or invalid".into());
    }
    Ok(())
}

#[async_trait]
impl LifecycleOutbox for PostgresManagedSessionRepository {
    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError> {
        sqlx::query(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
             ON CONFLICT (fact_id) DO NOTHING",
        )
        .bind(&fact.id)
        .bind(lifecycle_str(&fact))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn list_pending(
        &self,
        after: Option<&awaken_session_contract::LifecycleOutboxCursor>,
        limit: std::num::NonZeroU32,
    ) -> Result<awaken_session_contract::LifecycleOutboxBatch, SessionRepositoryError> {
        use awaken_session_contract::{LifecycleOutboxBatch, LifecycleOutboxCursor};
        let bound = i64::from(limit.get());
        let rows: Vec<(String, String, String)> = match after.map(LifecycleOutboxCursor::position) {
            Some((created_at, fact_id)) => sqlx::query_as(
                "SELECT created_at::TEXT, fact_id, data FROM managed_lifecycle_outbox WHERE (created_at, fact_id) > ($1::text::timestamptz, $2) ORDER BY created_at, fact_id LIMIT $3",
            ).bind(created_at).bind(fact_id).bind(bound).fetch_all(&self.pool).await,
            None => sqlx::query_as(
                "SELECT created_at::TEXT, fact_id, data FROM managed_lifecycle_outbox ORDER BY created_at, fact_id LIMIT $1",
            ).bind(bound).fetch_all(&self.pool).await,
        }.map_err(storage)?;
        let next = rows
            .last()
            .filter(|_| rows.len() == limit.get() as usize)
            .map(|(created_at, fact_id, _)| {
                LifecycleOutboxCursor::new(created_at.clone(), fact_id.clone())
            });
        let facts = rows
            .into_iter()
            .map(|(_, _, data)| decode_lifecycle(&data).map_err(corrupt))
            .collect::<Result<_, _>>()?;
        Ok(LifecycleOutboxBatch::new(facts, next))
    }

    async fn claim_lifecycle(
        &self,
        fact_id: &str,
        ttl: Duration,
    ) -> Result<Option<String>, SessionRepositoryError> {
        sqlx::query_scalar(
            "WITH pending AS (
                SELECT fact_id FROM managed_lifecycle_outbox WHERE fact_id = $1 FOR UPDATE SKIP LOCKED
             )
             INSERT INTO managed_lifecycle_outbox_claim (fact_id, token, lease_until)
             SELECT fact_id, gen_random_uuid(), clock_timestamp() + make_interval(secs => $2)
             FROM pending
             ON CONFLICT (fact_id) DO UPDATE
             SET token = EXCLUDED.token, lease_until = EXCLUDED.lease_until
             WHERE managed_lifecycle_outbox_claim.lease_until <= clock_timestamp()
             RETURNING token::text",
        )
        .bind(fact_id)
        .bind(ttl.as_secs_f64())
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)
    }

    async fn renew_lifecycle_claim(
        &self,
        fact_id: &str,
        claim: &str,
        ttl: Duration,
    ) -> Result<bool, SessionRepositoryError> {
        let result = sqlx::query(
            "UPDATE managed_lifecycle_outbox_claim
             SET lease_until = clock_timestamp() + make_interval(secs => $3)
             WHERE fact_id = $1 AND token = $2::text::uuid
               AND lease_until > clock_timestamp()",
        )
        .bind(fact_id)
        .bind(claim)
        .bind(ttl.as_secs_f64())
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(result.rows_affected() == 1)
    }

    async fn finish_lifecycle_claim(
        &self,
        fact_id: &str,
        claim: &str,
        delivered: bool,
    ) -> Result<(), SessionRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // Lock in the same order as claim_lifecycle. No connection is held
        // across callbacks or webhook I/O.
        sqlx::query("SELECT fact_id FROM managed_lifecycle_outbox WHERE fact_id = $1 FOR UPDATE")
            .bind(fact_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        let settled = sqlx::query(
            "DELETE FROM managed_lifecycle_outbox_claim WHERE fact_id = $1 AND token = $2::text::uuid
             AND lease_until > clock_timestamp()",
        )
        .bind(fact_id)
        .bind(claim)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        if delivered && settled.rows_affected() == 1 {
            sqlx::query("DELETE FROM managed_lifecycle_outbox WHERE fact_id = $1")
                .bind(fact_id)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)
    }

    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        sqlx::query_scalar::<_, String>(
            "SELECT data FROM managed_lifecycle_outbox ORDER BY created_at, fact_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .iter()
        .map(|data| decode_lifecycle(data).map_err(corrupt))
        .collect()
    }

    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError> {
        sqlx::query("DELETE FROM managed_lifecycle_outbox WHERE fact_id = $1")
            .bind(fact_id)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }
}
