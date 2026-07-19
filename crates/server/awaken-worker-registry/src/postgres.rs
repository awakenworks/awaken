use async_trait::async_trait;
use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerRegistration,
};
use sqlx::PgConnection;
use sqlx::postgres::PgPool;

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
    pub async fn connect(url: &str) -> Result<Self, RegistryError> {
        let pool = PgPool::connect(url).await.map_err(persist)?;
        Self::with_pool(pool).await
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
        let encoded: Option<String> = sqlx::query_scalar(
            "SELECT record_json FROM worker_registry_worker WHERE worker_id = $1 FOR UPDATE",
        )
        .bind(worker_id)
        .fetch_optional(conn)
        .await
        .map_err(persist)?;
        encoded
            .map(|value| serde_json::from_str(&value).map_err(persist))
            .transpose()
    }

    async fn write(
        conn: &mut PgConnection,
        record: &RegisteredWorker,
    ) -> Result<(), RegistryError> {
        sqlx::query(
            "INSERT INTO worker_registry_worker \
                (worker_id, incarnation_id, generation, state, expires_at_ms, record_json) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT(worker_id) DO UPDATE SET \
                incarnation_id = excluded.incarnation_id, generation = excluded.generation, \
                state = excluded.state, expires_at_ms = excluded.expires_at_ms, \
                record_json = excluded.record_json",
        )
        .bind(&record.snapshot.identity.worker_id)
        .bind(&record.snapshot.identity.incarnation_id)
        .bind(record.snapshot.identity.generation as i64)
        .bind(transition::state_name(record.snapshot.state))
        .bind(record.snapshot.expires_at_ms as i64)
        .bind(serde_json::to_string(record).map_err(persist)?)
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
        let encoded: Option<String> = sqlx::query_scalar(
            "SELECT record_json FROM worker_registry_worker WHERE worker_id = $1",
        )
        .bind(worker_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(persist)?;
        encoded
            .map(|value| serde_json::from_str(&value).map_err(persist))
            .transpose()
    }

    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        let encoded: Vec<String> =
            sqlx::query_scalar("SELECT record_json FROM worker_registry_worker ORDER BY worker_id")
                .fetch_all(&self.pool)
                .await
                .map_err(persist)?;
        encoded
            .into_iter()
            .map(|value| serde_json::from_str(&value).map_err(persist))
            .collect()
    }

    async fn expire(&self, now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError> {
        let mut tx = self.pool.begin().await.map_err(persist)?;
        let encoded: Vec<String> = sqlx::query_scalar(
            "SELECT record_json FROM worker_registry_worker \
             WHERE state IN ('starting', 'ready', 'draining') AND expires_at_ms <= $1 \
             FOR UPDATE SKIP LOCKED",
        )
        .bind(now_ms as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(persist)?;
        let mut expired = Vec::new();
        for value in encoded {
            let current: RegisteredWorker = serde_json::from_str(&value).map_err(persist)?;
            if let Some(next) = transition::expire(&current, now_ms) {
                expired.push(next.snapshot.identity.clone());
                Self::write(&mut tx, &next).await?;
            }
        }
        tx.commit().await.map_err(persist)?;
        Ok(expired)
    }
}
