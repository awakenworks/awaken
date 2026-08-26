use async_trait::async_trait;
use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerObservationSource, WorkerRegistration,
};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{PgConnection, Row};

use crate::codec::{EncodedWorkerRow, WORKER_COLUMNS, decode, encode_json};
use crate::durable_i64;
use crate::schema::{NS, registry_bundle};
use crate::transition;

pub struct PostgresWorkerDirectory {
    pool: PgPool,
}

enum Mutation {
    Heartbeat {
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    },
    BeginDrain {
        deadline_ms: u64,
    },
    Quiesce,
    Deregister,
}

impl PostgresWorkerDirectory {
    fn pool_options(max_connections: u32) -> PgPoolOptions {
        PgPoolOptions::new().max_connections(max_connections)
    }

    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, RegistryError> {
        let pool = Self::pool_options(max_connections)
            .connect(url)
            .await
            .map_err(persist)?;
        Self::with_pool(pool).await
    }

    /// Connect to a registry schema already applied by the deployment migration
    /// phase. This path verifies the ledger and never executes DDL.
    pub async fn connect_existing(url: &str, max_connections: u32) -> Result<Self, RegistryError> {
        let pool = Self::pool_options(max_connections)
            .connect(url)
            .await
            .map_err(persist)?;
        Self::with_existing_pool(pool).await
    }

    pub async fn with_pool(pool: PgPool) -> Result<Self, RegistryError> {
        let bundle = registry_bundle().map_err(persist)?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(persist)?
            .run_bundle(&bundle)
            .await
            .map_err(persist)?;
        Ok(Self { pool })
    }

    /// Wrap a shared pool after verifying its externally-owned migration ledger.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, RegistryError> {
        let bundle = registry_bundle().map_err(persist)?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(persist)?
            .verify_bundle(&bundle)
            .await
            .map_err(persist)?;
        Ok(Self { pool })
    }

    #[must_use]
    pub fn pool(&self) -> PgPool {
        self.pool.clone()
    }

    async fn lock_slot(conn: &mut PgConnection, worker_id: &str) -> Result<(), RegistryError> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(worker_id)
            .execute(conn)
            .await
            .map_err(persist)?;
        Ok(())
    }

    async fn read(
        conn: &mut PgConnection,
        worker_id: &str,
    ) -> Result<Option<RegisteredWorker>, RegistryError> {
        let row = sqlx::query(&format!(
            "SELECT {WORKER_COLUMNS} FROM worker_registry_worker WHERE worker_id = $1 FOR UPDATE"
        ))
        .bind(worker_id)
        .fetch_optional(conn)
        .await
        .map_err(persist)?;
        row.map(|row| decode(encoded_row(&row))).transpose()
    }

    async fn write(
        conn: &mut PgConnection,
        record: &RegisteredWorker,
    ) -> Result<(), RegistryError> {
        let generation = durable_i64("generation", record.snapshot.identity.generation)?;
        let expires_at_ms = durable_i64("expires_at_ms", record.snapshot.expires_at_ms)?;
        let in_flight = i64::from(record.snapshot.in_flight);
        sqlx::query(
            "INSERT INTO worker_registry_worker \
                (worker_id, incarnation_id, generation, state, manifest_json, \
                 capability_fingerprint, in_flight, warm_environment_shapes_json, \
                 credential_observations_json, acp_capability_observations_json, \
                 expires_at_ms, heartbeat_sequence, observation_sequence, registered_at_ms, \
                 heartbeat_at_ms, drain_deadline_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
             ON CONFLICT(worker_id) DO UPDATE SET \
                incarnation_id = excluded.incarnation_id, generation = excluded.generation, \
                state = excluded.state, manifest_json = excluded.manifest_json, \
                capability_fingerprint = excluded.capability_fingerprint, \
                in_flight = excluded.in_flight, \
                warm_environment_shapes_json = excluded.warm_environment_shapes_json, \
                credential_observations_json = excluded.credential_observations_json, \
                acp_capability_observations_json = excluded.acp_capability_observations_json, \
                expires_at_ms = excluded.expires_at_ms, \
                heartbeat_sequence = excluded.heartbeat_sequence, \
                observation_sequence = excluded.observation_sequence, \
                registered_at_ms = excluded.registered_at_ms, \
                heartbeat_at_ms = excluded.heartbeat_at_ms, \
                drain_deadline_ms = excluded.drain_deadline_ms",
        )
        .bind(&record.snapshot.identity.worker_id)
        .bind(&record.snapshot.identity.incarnation_id)
        .bind(generation)
        .bind(transition::state_name(record.snapshot.state))
        .bind(encode_json("manifest_json", &record.snapshot.manifest)?)
        .bind(&record.snapshot.capability_fingerprint)
        .bind(in_flight)
        .bind(encode_json(
            "warm_environment_shapes_json",
            &record.snapshot.warm_environment_shapes,
        )?)
        .bind(encode_json(
            "credential_observations_json",
            &record.snapshot.credential_observations,
        )?)
        .bind(encode_json(
            "acp_capability_observations_json",
            &record.snapshot.acp_capability_observations,
        )?)
        .bind(expires_at_ms)
        .bind(durable_i64(
            "heartbeat_sequence",
            record.heartbeat_sequence,
        )?)
        .bind(durable_i64(
            "observation_sequence",
            record.observation_sequence,
        )?)
        .bind(durable_i64("registered_at_ms", record.registered_at_ms)?)
        .bind(durable_i64("heartbeat_at_ms", record.heartbeat_at_ms)?)
        .bind(
            record
                .drain_deadline_ms
                .map(|value| durable_i64("drain_deadline_ms", value))
                .transpose()?,
        )
        .execute(conn)
        .await
        .map_err(persist)?;
        Ok(())
    }

    async fn mutate(
        &self,
        identity: &WorkerIdentity,
        mutation: Mutation,
    ) -> Result<RegistryMutation, RegistryError> {
        let mut tx = self.pool.begin().await.map_err(persist)?;
        Self::lock_slot(&mut tx, &identity.worker_id).await?;
        let current = Self::read(&mut tx, &identity.worker_id).await?;
        let (next, outcome) = match mutation {
            Mutation::Heartbeat {
                heartbeat,
                now_ms,
                ttl_ms,
            } => transition::heartbeat(current.as_ref(), identity, heartbeat, now_ms, ttl_ms),
            Mutation::BeginDrain { deadline_ms } => {
                transition::begin_drain(current.as_ref(), identity, deadline_ms)
            }
            Mutation::Quiesce => transition::quiesce(current.as_ref(), identity),
            Mutation::Deregister => transition::deregister(current.as_ref(), identity),
        };
        if let Some(next) = next {
            Self::write(&mut tx, &next).await?;
        }
        tx.commit().await.map_err(persist)?;
        Ok(outcome)
    }
}

fn persist(error: impl std::fmt::Display) -> RegistryError {
    RegistryError::Persistence(error.to_string())
}

#[async_trait]
impl WorkerObservationSource for PostgresWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        let rows = sqlx::query(&format!(
            "SELECT {WORKER_COLUMNS} FROM worker_registry_worker ORDER BY worker_id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(persist)?;
        rows.into_iter()
            .map(|row| decode(encoded_row(&row)))
            .collect()
    }
}

#[async_trait]
impl WorkerDirectory for PostgresWorkerDirectory {
    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        let worker_id = registration.worker_id.clone();
        let mut tx = self.pool.begin().await.map_err(persist)?;
        Self::lock_slot(&mut tx, &worker_id).await?;
        let current = Self::read(&mut tx, &worker_id).await?;
        let (record, changed) =
            transition::register(current.as_ref(), registration, now_ms, ttl_ms)?;
        if changed {
            Self::write(&mut tx, &record).await?;
        }
        tx.commit().await.map_err(persist)?;
        Ok(record)
    }

    async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(
            identity,
            Mutation::Heartbeat {
                heartbeat,
                now_ms,
                ttl_ms,
            },
        )
        .await
    }

    async fn begin_drain(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, Mutation::BeginDrain { deadline_ms })
            .await
    }

    async fn mark_quiesced(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, Mutation::Quiesce).await
    }

    async fn deregister(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, Mutation::Deregister).await
    }

    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        let row = sqlx::query(&format!(
            "SELECT {WORKER_COLUMNS} FROM worker_registry_worker WHERE worker_id = $1"
        ))
        .bind(worker_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(persist)?;
        row.map(|row| decode(encoded_row(&row))).transpose()
    }

    async fn expire(&self, now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError> {
        let durable_now_ms = durable_i64("now_ms", now_ms)?;
        let mut tx = self.pool.begin().await.map_err(persist)?;
        let rows = sqlx::query(&format!(
            "SELECT {WORKER_COLUMNS} FROM worker_registry_worker \
             WHERE state IN ('starting', 'ready', 'draining') AND expires_at_ms <= $1 \
             FOR UPDATE SKIP LOCKED"
        ))
        .bind(durable_now_ms)
        .fetch_all(&mut *tx)
        .await
        .map_err(persist)?;
        let mut expired = Vec::new();
        for row in rows {
            let current = decode(encoded_row(&row))?;
            if let Some(next) = transition::expire(&current, now_ms) {
                expired.push(next.snapshot.identity.clone());
                Self::write(&mut tx, &next).await?;
            }
        }
        tx.commit().await.map_err(persist)?;
        Ok(expired)
    }
}

fn encoded_row(row: &sqlx::postgres::PgRow) -> EncodedWorkerRow {
    EncodedWorkerRow {
        worker_id: row.get("worker_id"),
        incarnation_id: row.get("incarnation_id"),
        generation: row.get("generation"),
        state: row.get("state"),
        manifest_json: row.get("manifest_json"),
        capability_fingerprint: row.get("capability_fingerprint"),
        in_flight: row.get("in_flight"),
        warm_environment_shapes_json: row.get("warm_environment_shapes_json"),
        credential_observations_json: row.get("credential_observations_json"),
        acp_capability_observations_json: row.get("acp_capability_observations_json"),
        expires_at_ms: row.get("expires_at_ms"),
        heartbeat_sequence: row.get("heartbeat_sequence"),
        observation_sequence: row.get("observation_sequence"),
        registered_at_ms: row.get("registered_at_ms"),
        heartbeat_at_ms: row.get("heartbeat_at_ms"),
        drain_deadline_ms: row.get("drain_deadline_ms"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_budget_is_the_explicit_composition_input() {
        // Cause/effect graph: C1=composition-resolved connection budget;
        // E1=the registry pool uses exactly C1. Decision table: R1 budget 1 ->
        // one connection; R2 production pressure budget 32 -> 32 connections.
        // There is deliberately no omitted/default rule: every production caller
        // must pass the same deployment policy used by sibling runtime stores.
        assert_eq!(
            PostgresWorkerDirectory::pool_options(1).get_max_connections(),
            1,
            "R1"
        );
        assert_eq!(
            PostgresWorkerDirectory::pool_options(32).get_max_connections(),
            32,
            "R2"
        );
    }
}
