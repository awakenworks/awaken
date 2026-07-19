use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerRegistration,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::schema::{NS, registry_bundle};
use crate::transition;

pub struct SqliteWorkerDirectory {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteWorkerDirectory {
    pub fn open(path: &str) -> Result<Self, RegistryError> {
        Self::from_connection(Connection::open(path).map_err(persist)?)
    }

    pub fn open_in_memory() -> Result<Self, RegistryError> {
        Self::from_connection(Connection::open_in_memory().map_err(persist)?)
    }

    fn from_connection(conn: Connection) -> Result<Self, RegistryError> {
        let bundle = registry_bundle().map_err(persist)?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(persist)?
            .run_bundle(&conn, &bundle)
            .map_err(persist)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn read(
        tx: &Transaction<'_>,
        worker_id: &str,
    ) -> Result<Option<RegisteredWorker>, RegistryError> {
        let encoded = tx
            .query_row(
                "SELECT record_json FROM worker_registry_worker WHERE worker_id = ?1",
                params![worker_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(persist)?;
        encoded
            .map(|value| serde_json::from_str(&value).map_err(persist))
            .transpose()
    }

    fn write(tx: &Transaction<'_>, record: &RegisteredWorker) -> Result<(), RegistryError> {
        tx.execute(
            "INSERT INTO worker_registry_worker \
                (worker_id, incarnation_id, generation, state, expires_at_ms, record_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(worker_id) DO UPDATE SET \
                incarnation_id = excluded.incarnation_id, generation = excluded.generation, \
                state = excluded.state, expires_at_ms = excluded.expires_at_ms, \
                record_json = excluded.record_json",
            params![
                record.snapshot.identity.worker_id,
                record.snapshot.identity.incarnation_id,
                record.snapshot.identity.generation,
                transition::state_name(record.snapshot.state),
                record.snapshot.expires_at_ms,
                serde_json::to_string(record).map_err(persist)?,
            ],
        )
        .map_err(persist)?;
        Ok(())
    }

    fn mutate(
        &self,
        identity: &WorkerIdentity,
        decide: impl FnOnce(Option<&RegisteredWorker>) -> (Option<RegisteredWorker>, RegistryMutation),
    ) -> Result<RegistryMutation, RegistryError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))?;
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
impl WorkerDirectory for SqliteWorkerDirectory {
    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        let worker_id = registration.worker_id.clone();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))?;
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
    }

    async fn begin_drain(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| {
            transition::begin_drain(current, identity, deadline_ms)
        })
    }

    async fn mark_quiesced(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| transition::quiesce(current, identity))
    }

    async fn deregister(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| {
            transition::deregister(current, identity)
        })
    }

    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))?;
        let tx = conn.transaction().map_err(persist)?;
        Self::read(&tx, worker_id)
    }

    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))?;
        let mut stmt = conn
            .prepare("SELECT record_json FROM worker_registry_worker ORDER BY worker_id")
            .map_err(persist)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(persist)?;
        rows.map(|row| {
            let encoded = row.map_err(persist)?;
            serde_json::from_str(&encoded).map_err(persist)
        })
        .collect()
    }

    async fn expire(&self, now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(persist)?;
        let encoded = {
            let mut stmt = tx
                .prepare(
                    "SELECT record_json FROM worker_registry_worker \
                     WHERE state IN ('starting', 'ready', 'draining') AND expires_at_ms <= ?1",
                )
                .map_err(persist)?;
            let rows = stmt
                .query_map(params![now_ms], |row| row.get::<_, String>(0))
                .map_err(persist)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(persist)?
        };
        let mut expired = Vec::new();
        for value in encoded {
            let current: RegisteredWorker = serde_json::from_str(&value).map_err(persist)?;
            if let Some(next) = transition::expire(&current, now_ms) {
                expired.push(next.snapshot.identity.clone());
                Self::write(&tx, &next)?;
            }
        }
        tx.commit().map_err(persist)?;
        Ok(expired)
    }
}
