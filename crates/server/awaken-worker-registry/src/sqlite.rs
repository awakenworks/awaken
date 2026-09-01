use async_trait::async_trait;
use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerObservationSource, WorkerRegistration,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::path::Path;

use crate::codec::{EncodedWorkerRow, WORKER_COLUMNS, decode, encode_json};
use crate::durable_i64;
use crate::schema::{BUNDLE_ID, NS, converged_registry_bundle, selected_registry_bundle};
use crate::transition;

pub struct SqliteWorkerDirectory {
    conn: awaken_sqlite_runtime::SharedSqliteConnection,
}

impl SqliteWorkerDirectory {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RegistryError> {
        Self::from_connection(
            awaken_sqlite_runtime::SqliteConnectionFactory::file(path)
                .open()
                .map_err(persist)?,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, RegistryError> {
        Self::from_connection(
            awaken_sqlite_runtime::SqliteConnectionFactory::memory()
                .open()
                .map_err(persist)?,
        )
    }

    fn from_connection(conn: Connection) -> Result<Self, RegistryError> {
        let ledger_exists = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                params![format!("{NS}_schema_migrations")],
                |row| row.get::<_, bool>(0),
            )
            .map_err(persist)?;
        let v1_checksum = if ledger_exists {
            conn.query_row(
                &format!(
                    "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = ?1 AND version = 1"
                ),
                params![BUNDLE_ID],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(persist)?
        } else {
            None
        };
        let published = selected_registry_bundle(v1_checksum.as_deref()).map_err(persist)?;
        let converged = converged_registry_bundle().map_err(persist)?;
        let runner = awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(persist)?;
        runner
            .run_bundle(&conn, &published)
            .and_then(|_| runner.run_bundle(&conn, &converged))
            .map_err(persist)?;
        Ok(Self {
            conn: awaken_sqlite_runtime::SharedSqliteConnection::new(conn),
        })
    }

    fn read(
        tx: &Transaction<'_>,
        worker_id: &str,
    ) -> Result<Option<RegisteredWorker>, RegistryError> {
        let encoded = tx
            .query_row(
                &format!(
                    "SELECT {WORKER_COLUMNS} FROM worker_registry_worker WHERE worker_id = ?1"
                ),
                params![worker_id],
                encoded_row,
            )
            .optional()
            .map_err(persist)?;
        encoded.map(decode).transpose()
    }

    fn write(tx: &Transaction<'_>, record: &RegisteredWorker) -> Result<(), RegistryError> {
        let generation = durable_i64("generation", record.snapshot.identity.generation)?;
        let expires_at_ms = durable_i64("expires_at_ms", record.snapshot.expires_at_ms)?;
        let in_flight = i64::from(record.snapshot.in_flight);
        tx.execute(
            "INSERT INTO worker_registry_worker \
                (worker_id, incarnation_id, generation, state, manifest_json, \
                 capability_fingerprint, in_flight, warm_environment_shapes_json, \
                 credential_observations_json, acp_capability_observations_json, \
                 expires_at_ms, heartbeat_sequence, observation_sequence, registered_at_ms, \
                 heartbeat_at_ms, drain_deadline_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) \
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
            params![
                record.snapshot.identity.worker_id,
                record.snapshot.identity.incarnation_id,
                generation,
                transition::state_name(record.snapshot.state),
                encode_json("manifest_json", &record.snapshot.manifest)?,
                record.snapshot.capability_fingerprint,
                in_flight,
                encode_json(
                    "warm_environment_shapes_json",
                    &record.snapshot.warm_environment_shapes
                )?,
                encode_json(
                    "credential_observations_json",
                    &record.snapshot.credential_observations
                )?,
                encode_json(
                    "acp_capability_observations_json",
                    &record.snapshot.acp_capability_observations
                )?,
                expires_at_ms,
                durable_i64("heartbeat_sequence", record.heartbeat_sequence)?,
                durable_i64("observation_sequence", record.observation_sequence)?,
                durable_i64("registered_at_ms", record.registered_at_ms)?,
                durable_i64("heartbeat_at_ms", record.heartbeat_at_ms)?,
                record
                    .drain_deadline_ms
                    .map(|value| durable_i64("drain_deadline_ms", value))
                    .transpose()?,
            ],
        )
        .map_err(persist)?;
        Ok(())
    }

    async fn with_connection<T, F>(&self, operation: F) -> Result<T, RegistryError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, RegistryError> + Send + 'static,
    {
        awaken_sqlite_runtime::with_connection(self.conn.clone(), operation)
            .await
            .map_err(persist)?
    }

    fn mutate(
        conn: &mut Connection,
        identity: &WorkerIdentity,
        decide: impl FnOnce(Option<&RegisteredWorker>) -> (Option<RegisteredWorker>, RegistryMutation),
    ) -> Result<RegistryMutation, RegistryError> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(persist)?;
        let current = Self::read(&tx, &identity.worker_id)?;
        let (next, outcome) = decide(current.as_ref());
        if let Some(next) = next {
            Self::write(&tx, &next)?;
        }
        tx.commit().map_err(persist)?;
        Ok(outcome)
    }
}

fn persist(error: impl std::fmt::Display) -> RegistryError {
    RegistryError::Persistence(error.to_string())
}

#[async_trait]
impl WorkerObservationSource for SqliteWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        self.with_connection(|conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {WORKER_COLUMNS} FROM worker_registry_worker ORDER BY worker_id"
                ))
                .map_err(persist)?;
            let rows = stmt.query_map([], encoded_row).map_err(persist)?;
            rows.map(|row| {
                let encoded = row.map_err(persist)?;
                decode(encoded)
            })
            .collect()
        })
        .await
    }
}

#[async_trait]
impl WorkerDirectory for SqliteWorkerDirectory {
    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        self.with_connection(move |conn| {
            let worker_id = registration.worker_id.clone();
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(persist)?;
            let current = Self::read(&tx, &worker_id)?;
            let (record, changed) =
                transition::register(current.as_ref(), registration, now_ms, ttl_ms)?;
            if changed {
                Self::write(&tx, &record)?;
            }
            tx.commit().map_err(persist)?;
            Ok(record)
        })
        .await
    }

    async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        let identity = identity.clone();
        self.with_connection(move |conn| {
            Self::mutate(conn, &identity, |current| {
                transition::heartbeat(current, &identity, heartbeat, now_ms, ttl_ms)
            })
        })
        .await
    }

    async fn begin_drain(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        let identity = identity.clone();
        self.with_connection(move |conn| {
            Self::mutate(conn, &identity, |current| {
                transition::begin_drain(current, &identity, deadline_ms)
            })
        })
        .await
    }

    async fn mark_quiesced(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        let identity = identity.clone();
        self.with_connection(move |conn| {
            Self::mutate(conn, &identity, |current| {
                transition::quiesce(current, &identity)
            })
        })
        .await
    }

    async fn deregister(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        let identity = identity.clone();
        self.with_connection(move |conn| {
            Self::mutate(conn, &identity, |current| {
                transition::deregister(current, &identity)
            })
        })
        .await
    }

    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        let worker_id = worker_id.to_string();
        self.with_connection(move |conn| {
            let tx = conn.transaction().map_err(persist)?;
            Self::read(&tx, &worker_id)
        })
        .await
    }

    async fn expire(&self, now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError> {
        let durable_now_ms = durable_i64("now_ms", now_ms)?;
        self.with_connection(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(persist)?;
            let encoded = {
                let mut stmt = tx
                    .prepare(&format!(
                        "SELECT {WORKER_COLUMNS} FROM worker_registry_worker \
                         WHERE state IN ('starting', 'ready', 'draining') AND expires_at_ms <= ?1",
                    ))
                    .map_err(persist)?;
                let rows = stmt
                    .query_map(params![durable_now_ms], encoded_row)
                    .map_err(persist)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(persist)?
            };
            let mut expired = Vec::new();
            for value in encoded {
                let current = decode(value)?;
                if let Some(next) = transition::expire(&current, now_ms) {
                    expired.push(next.snapshot.identity.clone());
                    Self::write(&tx, &next)?;
                }
            }
            tx.commit().map_err(persist)?;
            Ok(expired)
        })
        .await
    }
}

fn encoded_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EncodedWorkerRow> {
    Ok(EncodedWorkerRow {
        worker_id: row.get(0)?,
        incarnation_id: row.get(1)?,
        generation: row.get(2)?,
        state: row.get(3)?,
        manifest_json: row.get(4)?,
        capability_fingerprint: row.get(5)?,
        in_flight: row.get(6)?,
        warm_environment_shapes_json: row.get(7)?,
        credential_observations_json: row.get(8)?,
        acp_capability_observations_json: row.get(9)?,
        expires_at_ms: row.get(10)?,
        heartbeat_sequence: row.get(11)?,
        observation_sequence: row.get(12)?,
        registered_at_ms: row.get(13)?,
        heartbeat_at_ms: row.get(14)?,
        drain_deadline_ms: row.get(15)?,
    })
}

#[cfg(test)]
mod scheduler_isolation_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Worker-registry scheduler-isolation cause/effect graph: C1 a synchronous
    /// SQLite owner holds the registry connection; C2 two async observations
    /// contend on a two-worker Tokio runtime. Effects: E1 registry waiters stay
    /// on the blocking pool; E2 the heartbeat timer fires before the owner
    /// releases at 250 ms; E3 both observations complete after release.
    /// Decision rule W1=C1+C2=>E1+E2+E3. This adapter-level rule proves the
    /// registry delegates to the canonical SQLite scheduler boundary rather
    /// than merely inheriting its connection PRAGMAs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_contention_cannot_starve_worker_heartbeat_timers() {
        let directory = Arc::new(SqliteWorkerDirectory::open_in_memory().expect("directory"));
        let held = directory.conn.clone();
        let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
        let holder = std::thread::spawn(move || {
            let _guard = held.lock().expect("W1 connection lock");
            held_tx.send(()).expect("W1 announce held connection");
            std::thread::sleep(Duration::from_millis(250));
        });
        held_rx.recv().expect("W1 connection held");

        let started = Instant::now();
        let timer = tokio::spawn(async move {
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
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let elapsed = tokio::time::timeout(Duration::from_millis(100), timer)
            .await
            .expect("W1/E1-E2 heartbeat timer remains schedulable")
            .expect("W1 timer task");
        assert!(
            elapsed < Duration::from_millis(100),
            "W1/E2 timer fired after {elapsed:?}; registry blocked runtime workers"
        );

        holder.join().expect("W1 release connection");
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
