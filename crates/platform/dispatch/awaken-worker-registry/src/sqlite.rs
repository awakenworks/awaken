// @formal entrypoint worker.incarnation_generation_fences_stale_processes
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )
)]
use async_trait::async_trait;
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;
use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerObservationSource, WorkerProcessExit, WorkerRegistration,
};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnection, SqliteRow};
use std::path::Path;

use crate::codec::{EncodedWorkerRow, WORKER_COLUMNS, current_workers_query, decode, encode_json};
use crate::durable_i64;
use crate::schema::{BUNDLE_ID, NS, converged_registry_bundle, selected_registry_bundle};
use crate::transition;

pub struct SqliteWorkerDirectory {
    pool: EmbeddedPool,
}

impl SqliteWorkerDirectory {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, RegistryError> {
        Self::from_pool(EmbeddedSqlite::file(path).open().await.map_err(persist)?).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory() -> Result<Self, RegistryError> {
        Self::from_pool(EmbeddedSqlite::memory().open().await.map_err(persist)?).await
    }

    async fn from_pool(pool: EmbeddedPool) -> Result<Self, RegistryError> {
        let ledger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        )
        .bind(format!("{NS}_schema_migrations"))
        .fetch_one(&pool)
        .await
        .map_err(persist)?;
        let v1_checksum: Option<String> = if ledger_exists {
            sqlx::query_scalar(&format!(
                "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = ?1 AND version = 1"
            ))
            .bind(BUNDLE_ID)
            .fetch_optional(&pool)
            .await
            .map_err(persist)?
        } else {
            None
        };
        let published = selected_registry_bundle(v1_checksum.as_deref()).map_err(persist)?;
        let converged = converged_registry_bundle().map_err(persist)?;
        let runner =
            SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).map_err(persist)?;
        awaken_sqlite_pool::held(runner.run_bundle(&published))
            .await
            .map_err(persist)?;
        awaken_sqlite_pool::held(runner.run_bundle(&converged))
            .await
            .map_err(persist)?;
        Ok(Self { pool })
    }

    async fn read(
        conn: &mut SqliteConnection,
        worker_id: &str,
    ) -> Result<Option<RegisteredWorker>, RegistryError> {
        let encoded = sqlx::query(&format!(
            "SELECT {WORKER_COLUMNS} FROM worker_registry_worker WHERE worker_id = ?1 ORDER BY generation DESC LIMIT 1"
        ))
        .bind(worker_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(persist)?
        .map(|row| encoded_row(&row))
        .transpose()
        .map_err(persist)?;
        encoded.map(decode).transpose()
    }

    async fn read_incarnation(
        conn: &mut SqliteConnection,
        identity: &WorkerIdentity,
    ) -> Result<Option<RegisteredWorker>, RegistryError> {
        let generation = durable_i64("generation", identity.generation)?;
        let encoded = sqlx::query(&format!(
            "SELECT {WORKER_COLUMNS} FROM worker_registry_worker WHERE worker_id = ?1 AND generation = ?2 AND incarnation_id = ?3"
        ))
        .bind(&identity.worker_id)
        .bind(generation)
        .bind(&identity.incarnation_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(persist)?
        .map(|row| encoded_row(&row))
        .transpose()
        .map_err(persist)?;
        encoded.map(decode).transpose()
    }

    async fn write(
        conn: &mut SqliteConnection,
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
                 heartbeat_at_ms, drain_deadline_ms, process_exit_json, create_ledger_json, \
                 source_retention_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19) \
             ON CONFLICT(worker_id, generation) DO UPDATE SET \
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
                drain_deadline_ms = excluded.drain_deadline_ms, \
                process_exit_json = excluded.process_exit_json, \
                create_ledger_json = excluded.create_ledger_json, \
                source_retention_json = excluded.source_retention_json",
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
        .bind(
            record
                .process_exit
                .as_ref()
                .map(|receipt| encode_json("process_exit_json", receipt))
                .transpose()?,
        )
        .bind(
            record
                .create_ledger
                .as_ref()
                .map(|ledger| encode_json("create_ledger_json", ledger))
                .transpose()?,
        )
        .bind(
            (!record.source_retention.is_empty())
                .then(|| encode_json("source_retention_json", &record.source_retention))
                .transpose()?,
        )
        .execute(&mut *conn)
        .await
        .map_err(persist)?;
        Ok(())
    }

    async fn mutate(
        &self,
        identity: &WorkerIdentity,
        decide: impl FnOnce(
            Option<&RegisteredWorker>,
        )
            -> Result<(Option<RegisteredWorker>, RegistryMutation), RegistryError>,
    ) -> Result<RegistryMutation, RegistryError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(persist)?;
        let current = Self::read(&mut tx, &identity.worker_id).await?;
        let (next, outcome) = decide(current.as_ref())?;
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
impl WorkerObservationSource for SqliteWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        sqlx::query(&current_workers_query())
            .fetch_all(&self.pool)
            .await
            .map_err(persist)?
            .iter()
            .map(|row| decode(encoded_row(row).map_err(persist)?))
            .collect()
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerCurrentSource for SqliteWorkerDirectory {
    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        let mut tx = self.pool.begin().await.map_err(persist)?;
        Self::read(&mut tx, worker_id).await
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for SqliteWorkerDirectory {
    async fn incarnation(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<Option<RegisteredWorker>, RegistryError> {
        let mut tx = self.pool.begin().await.map_err(persist)?;
        Self::read_incarnation(&mut tx, identity).await
    }
}

#[async_trait]
impl WorkerDirectory for SqliteWorkerDirectory {
    async fn lock_incarnation(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<Option<awaken_worker_contract::WorkerIncarnationGuard>, RegistryError> {
        // The guard holds the store's one connection inside an open `BEGIN
        // IMMEDIATE` transaction until it is released: no other operation on
        // this store (or writer on the same file) runs meanwhile. Release and
        // every early return roll it back; dropping the guard drops the
        // transaction, which rolls back before the connection is reused.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(persist)?;
        let current = match Self::read(&mut tx, &identity.worker_id).await {
            Ok(current) => current.filter(|r| r.snapshot.identity == *identity),
            Err(error) => {
                tx.rollback().await.map_err(persist)?;
                return Err(error);
            }
        };
        let Some(record) = current else {
            tx.rollback().await.map_err(persist)?;
            return Ok(None);
        };
        Ok(Some(
            awaken_worker_contract::WorkerIncarnationGuard::new_releasing(
                record.snapshot,
                async move { tx.rollback().await.map_err(|e| e.to_string()) },
            ),
        ))
    }
    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        let worker_id = registration.worker_id.clone();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(persist)?;
        let current = Self::read(&mut tx, &worker_id).await?;
        let (incarnation_seen, complete): (bool, bool) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM worker_registry_worker WHERE worker_id=?1 AND incarnation_id=?2), \
             EXISTS(SELECT 1 FROM worker_registry_worker WHERE worker_id=?1 AND generation=1)",
        )
        .bind(&worker_id)
        .bind(&registration.incarnation_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(persist)?;
        let history = transition::RegistrationHistory {
            incarnation_seen,
            complete,
        };
        let (record, changed) =
            transition::register(current.as_ref(), history, registration, now_ms, ttl_ms)?;
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
        self.mutate(identity, |current| {
            transition::heartbeat(current, identity, heartbeat, now_ms, ttl_ms)
        })
        .await
    }

    async fn begin_drain(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| {
            Ok(transition::begin_drain(current, identity, deadline_ms))
        })
        .await
    }

    async fn mark_quiesced(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| {
            Ok(transition::quiesce(current, identity))
        })
        .await
    }

    async fn deregister(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| {
            Ok(transition::deregister(current, identity))
        })
        .await
    }

    async fn expire_incarnation(
        &self,
        identity: &WorkerIdentity,
        now_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        durable_i64("now_ms", now_ms)?;
        self.mutate(identity, |current| {
            Ok(transition::expire(current, identity, now_ms))
        })
        .await
    }

    async fn record_process_exit(
        &self,
        identity: &WorkerIdentity,
        receipt: WorkerProcessExit,
    ) -> Result<RegistryMutation, RegistryError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(persist)?;
        let record = Self::read_incarnation(&mut tx, identity).await?;
        let (next, outcome) = transition::record_process_exit(record.as_ref(), identity, receipt)?;
        if let Some(next) = next {
            Self::write(&mut tx, &next).await?;
        }
        tx.commit().await.map_err(persist)?;
        Ok(outcome)
    }
}

fn encoded_row(row: &SqliteRow) -> Result<EncodedWorkerRow, sqlx::Error> {
    Ok(EncodedWorkerRow {
        worker_id: row.try_get(0)?,
        incarnation_id: row.try_get(1)?,
        generation: row.try_get(2)?,
        state: row.try_get(3)?,
        manifest_json: row.try_get(4)?,
        capability_fingerprint: row.try_get(5)?,
        in_flight: row.try_get(6)?,
        warm_environment_shapes_json: row.try_get(7)?,
        credential_observations_json: row.try_get(8)?,
        acp_capability_observations_json: row.try_get(9)?,
        expires_at_ms: row.try_get(10)?,
        heartbeat_sequence: row.try_get(11)?,
        observation_sequence: row.try_get(12)?,
        registered_at_ms: row.try_get(13)?,
        heartbeat_at_ms: row.try_get(14)?,
        drain_deadline_ms: row.try_get(15)?,
        process_exit_json: row.try_get(16)?,
        create_ledger_json: row.try_get(17)?,
        source_retention_json: row.try_get(18)?,
    })
}

#[cfg(test)]
mod scheduler_isolation_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Worker-registry scheduler-isolation cause/effect graph: C1 another owner
    /// holds the registry's one pooled connection; C2 two async observations
    /// contend on a two-worker Tokio runtime. Effects: E1 registry waiters
    /// queue on pool admission without occupying a runtime worker; E2 the
    /// heartbeat timer fires before the owner releases; E3 both observations
    /// complete after release. Decision rule W1=C1+C2=>E1+E2+E3. This
    /// adapter-level rule proves the registry goes through the canonical
    /// embedded SQLite pool rather than merely inheriting its connection PRAGMAs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_contention_cannot_starve_worker_heartbeat_timers() {
        let directory = Arc::new(
            SqliteWorkerDirectory::open_in_memory()
                .await
                .expect("directory"),
        );
        let held = directory.pool.acquire().await.expect("W1 connection held");

        let started = Instant::now();
        let timer = tokio::spawn(async move {
            // awaken-allow(wait): W1 measures the timer itself; its prompt firing proves runtime workers are not blocked.
            tokio::time::sleep(Duration::from_millis(10)).await;
            started.elapsed()
        });
        let left = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.list().await }
        });
        let right = tokio::spawn({
            let directory = Arc::clone(&directory);
            async move { directory.list().await }
        });

        let elapsed = tokio::time::timeout(Duration::from_millis(100), timer)
            .await
            .expect("W1/E1-E2 heartbeat timer remains schedulable")
            .expect("W1 timer task");
        assert!(
            elapsed < Duration::from_millis(100),
            "W1/E2 timer fired after {elapsed:?}; registry blocked runtime workers"
        );
        assert!(
            !left.is_finished() && !right.is_finished(),
            "W1/E1 observations wait for the held connection"
        );

        drop(held);
        assert!(
            left.await
                .expect("left observation")
                .expect("W1/E3")
                .is_empty()
        );
        assert!(
            right
                .await
                .expect("right observation")
                .expect("W1/E3")
                .is_empty()
        );
    }
}
