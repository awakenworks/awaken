//! SQLite-backed implementation of [`MailboxStore`].

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::mailbox::{
    MailboxInterrupt, MailboxJob, MailboxJobOrigin, MailboxJobStatus, MailboxStore,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::StorageError;
use rusqlite::{Connection, Row, params};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

// ── SqliteMailboxStore ─────────────────────────────────────────────

/// SQLite-backed persistent mailbox store.
///
/// Uses WAL mode for concurrent read access. All writes are serialized
/// through `tokio::sync::Mutex` wrapping a single `rusqlite::Connection`.
pub struct SqliteMailboxStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteMailboxStore {
    /// Open (or create) a SQLite database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let conn =
            Connection::open(path).map_err(|e| StorageError::Io(format!("sqlite open: {e}")))?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        // Block on table creation — called once at startup.
        let rt_conn = store.conn.clone();
        // We cannot use async here, so access the lock via try_lock
        // since no one else holds it yet.
        {
            let guard = rt_conn.try_lock().expect("no contention at construction");
            Self::create_tables(&guard)?;
        }
        Ok(store)
    }

    /// Open an in-memory SQLite database (useful for tests).
    pub fn open_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| StorageError::Io(format!("sqlite open_memory: {e}")))?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        {
            let guard = store
                .conn
                .try_lock()
                .expect("no contention at construction");
            Self::create_tables(&guard)?;
        }
        Ok(store)
    }

    fn create_tables(conn: &Connection) -> Result<(), StorageError> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .map_err(|e| StorageError::Io(format!("pragma: {e}")))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS mailbox_jobs (
                job_id         TEXT PRIMARY KEY,
                mailbox_id     TEXT NOT NULL,
                agent_id       TEXT NOT NULL,
                messages       TEXT NOT NULL,
                origin         TEXT NOT NULL,
                sender_id      TEXT,
                parent_run_id  TEXT,
                request_extras TEXT,
                priority       INTEGER NOT NULL DEFAULT 128,
                dedupe_key     TEXT,
                generation     INTEGER NOT NULL DEFAULT 0,
                status         TEXT NOT NULL DEFAULT 'Queued',
                available_at   INTEGER NOT NULL DEFAULT 0,
                attempt_count  INTEGER NOT NULL DEFAULT 0,
                max_attempts   INTEGER NOT NULL DEFAULT 5,
                last_error     TEXT,
                claim_token    TEXT,
                claimed_by     TEXT,
                lease_until    INTEGER,
                created_at     INTEGER NOT NULL,
                updated_at     INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_mailbox_jobs_mailbox_status
                ON mailbox_jobs (mailbox_id, status);

            CREATE INDEX IF NOT EXISTS idx_mailbox_jobs_dedupe
                ON mailbox_jobs (mailbox_id, dedupe_key)
                WHERE dedupe_key IS NOT NULL;

            CREATE INDEX IF NOT EXISTS idx_mailbox_jobs_lease
                ON mailbox_jobs (status, lease_until)
                WHERE status = 'Claimed';

            CREATE TABLE IF NOT EXISTS mailbox_generations (
                mailbox_id         TEXT PRIMARY KEY,
                current_generation INTEGER NOT NULL DEFAULT 0
            );",
        )
        .map_err(|e| StorageError::Io(format!("create tables: {e}")))?;

        Ok(())
    }
}

// ── Serialization helpers ──────────────────────────────────────────

fn status_to_str(s: MailboxJobStatus) -> &'static str {
    match s {
        MailboxJobStatus::Queued => "Queued",
        MailboxJobStatus::Claimed => "Claimed",
        MailboxJobStatus::Accepted => "Accepted",
        MailboxJobStatus::Cancelled => "Cancelled",
        MailboxJobStatus::Superseded => "Superseded",
        MailboxJobStatus::DeadLetter => "DeadLetter",
    }
}

fn str_to_status(s: &str) -> Result<MailboxJobStatus, StorageError> {
    match s {
        "Queued" => Ok(MailboxJobStatus::Queued),
        "Claimed" => Ok(MailboxJobStatus::Claimed),
        "Accepted" => Ok(MailboxJobStatus::Accepted),
        "Cancelled" => Ok(MailboxJobStatus::Cancelled),
        "Superseded" => Ok(MailboxJobStatus::Superseded),
        "DeadLetter" => Ok(MailboxJobStatus::DeadLetter),
        other => Err(StorageError::Io(format!(
            "unknown MailboxJobStatus: {other}"
        ))),
    }
}

fn origin_to_str(o: MailboxJobOrigin) -> &'static str {
    match o {
        MailboxJobOrigin::User => "User",
        MailboxJobOrigin::A2A => "A2A",
        MailboxJobOrigin::Internal => "Internal",
    }
}

fn str_to_origin(s: &str) -> Result<MailboxJobOrigin, StorageError> {
    match s {
        "User" => Ok(MailboxJobOrigin::User),
        "A2A" => Ok(MailboxJobOrigin::A2A),
        "Internal" => Ok(MailboxJobOrigin::Internal),
        other => Err(StorageError::Io(format!(
            "unknown MailboxJobOrigin: {other}"
        ))),
    }
}

fn row_to_job(row: &Row<'_>) -> Result<MailboxJob, rusqlite::Error> {
    let messages_json: String = row.get("messages")?;
    let messages: Vec<Message> = serde_json::from_str(&messages_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;

    let status_str: String = row.get("status")?;
    let status = str_to_status(&status_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            11,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{e:?}"),
            )),
        )
    })?;

    let origin_str: String = row.get("origin")?;
    let origin = str_to_origin(&origin_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{e:?}"),
            )),
        )
    })?;

    let request_extras_json: Option<String> = row.get("request_extras")?;
    let request_extras: Option<Value> = request_extras_json
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?;

    let priority_i64: i64 = row.get("priority")?;
    let generation_i64: i64 = row.get("generation")?;
    let available_at_i64: i64 = row.get("available_at")?;
    let attempt_count_i64: i64 = row.get("attempt_count")?;
    let max_attempts_i64: i64 = row.get("max_attempts")?;
    let lease_until: Option<i64> = row.get("lease_until")?;
    let created_at_i64: i64 = row.get("created_at")?;
    let updated_at_i64: i64 = row.get("updated_at")?;

    Ok(MailboxJob {
        job_id: row.get("job_id")?,
        mailbox_id: row.get("mailbox_id")?,
        agent_id: row.get("agent_id")?,
        messages,
        origin,
        sender_id: row.get("sender_id")?,
        parent_run_id: row.get("parent_run_id")?,
        request_extras,
        priority: priority_i64 as u8,
        dedupe_key: row.get("dedupe_key")?,
        generation: generation_i64 as u64,
        status,
        available_at: available_at_i64 as u64,
        attempt_count: attempt_count_i64 as u32,
        max_attempts: max_attempts_i64 as u32,
        last_error: row.get("last_error")?,
        claim_token: row.get("claim_token")?,
        claimed_by: row.get("claimed_by")?,
        lease_until: lease_until.map(|v| v as u64),
        created_at: created_at_i64 as u64,
        updated_at: updated_at_i64 as u64,
    })
}

// ── MailboxStore impl ──────────────────────────────────────────────

#[async_trait]
impl MailboxStore for SqliteMailboxStore {
    async fn enqueue(&self, job: &MailboxJob) -> Result<(), StorageError> {
        let conn = self.conn.lock().await;

        // Dedupe check.
        if let Some(ref dk) = job.dedupe_key {
            let dup: bool = conn
                .prepare_cached(
                    "SELECT EXISTS(
                        SELECT 1 FROM mailbox_jobs
                        WHERE mailbox_id = ?1
                          AND dedupe_key = ?2
                          AND status NOT IN ('Accepted','Cancelled','Superseded','DeadLetter')
                    )",
                )
                .map_err(|e| StorageError::Io(format!("prepare dedupe: {e}")))?
                .query_row(params![job.mailbox_id, dk], |row| row.get::<_, bool>(0))
                .map_err(|e| StorageError::Io(format!("dedupe check: {e}")))?;

            if dup {
                return Err(StorageError::AlreadyExists(format!("dedupe_key={dk}")));
            }
        }

        // Auto-create generation row; fetch current generation.
        conn.execute(
            "INSERT INTO mailbox_generations (mailbox_id, current_generation)
             VALUES (?1, 0)
             ON CONFLICT (mailbox_id) DO NOTHING",
            params![job.mailbox_id],
        )
        .map_err(|e| StorageError::Io(format!("upsert generation: {e}")))?;

        let generation: i64 = conn
            .prepare_cached(
                "SELECT current_generation FROM mailbox_generations WHERE mailbox_id = ?1",
            )
            .map_err(|e| StorageError::Io(format!("prepare gen select: {e}")))?
            .query_row(params![job.mailbox_id], |row| row.get(0))
            .map_err(|e| StorageError::Io(format!("gen select: {e}")))?;

        let messages_json = serde_json::to_string(&job.messages)
            .map_err(|e| StorageError::Io(format!("serialize messages: {e}")))?;
        let request_extras_json = job
            .request_extras
            .as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()
            .map_err(|e| StorageError::Io(format!("serialize extras: {e}")))?;

        conn.execute(
            "INSERT INTO mailbox_jobs (
                job_id, mailbox_id, agent_id, messages, origin,
                sender_id, parent_run_id, request_extras,
                priority, dedupe_key, generation,
                status, available_at, attempt_count, max_attempts,
                last_error, claim_token, claimed_by, lease_until,
                created_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8,
                ?9, ?10, ?11,
                ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19,
                ?20, ?21
            )",
            params![
                job.job_id,
                job.mailbox_id,
                job.agent_id,
                messages_json,
                origin_to_str(job.origin),
                job.sender_id,
                job.parent_run_id,
                request_extras_json,
                job.priority as i64,
                job.dedupe_key,
                generation,
                status_to_str(MailboxJobStatus::Queued),
                job.available_at as i64,
                job.attempt_count as i64,
                job.max_attempts as i64,
                job.last_error,
                job.claim_token,
                job.claimed_by,
                job.lease_until.map(|v| v as i64),
                job.created_at as i64,
                job.updated_at as i64,
            ],
        )
        .map_err(|e| StorageError::Io(format!("insert job: {e}")))?;

        Ok(())
    }

    async fn claim(
        &self,
        mailbox_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<MailboxJob>, StorageError> {
        let conn = self.conn.lock().await;

        // Cannot claim while another job is already Claimed for this mailbox.
        let has_claimed: bool = conn
            .prepare_cached(
                "SELECT EXISTS(
                    SELECT 1 FROM mailbox_jobs
                    WHERE mailbox_id = ?1 AND status = 'Claimed'
                )",
            )
            .map_err(|e| StorageError::Io(format!("prepare claim check: {e}")))?
            .query_row(params![mailbox_id], |row| row.get::<_, bool>(0))
            .map_err(|e| StorageError::Io(format!("claim check: {e}")))?;

        if has_claimed {
            return Ok(vec![]);
        }

        // Find oldest Queued jobs eligible for claiming.
        let mut stmt = conn
            .prepare_cached(
                "SELECT job_id FROM mailbox_jobs
                 WHERE mailbox_id = ?1
                   AND status = 'Queued'
                   AND available_at <= ?2
                 ORDER BY priority ASC, created_at ASC
                 LIMIT ?3",
            )
            .map_err(|e| StorageError::Io(format!("prepare claim select: {e}")))?;

        let job_ids: Vec<String> = stmt
            .query_map(params![mailbox_id, now as i64, limit as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| StorageError::Io(format!("claim select: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Io(format!("claim collect: {e}")))?;

        if job_ids.is_empty() {
            return Ok(vec![]);
        }

        let token = Uuid::now_v7().to_string();
        let lease_until = now + lease_ms;

        // Update each job to Claimed.
        let mut update_stmt = conn
            .prepare_cached(
                "UPDATE mailbox_jobs
                 SET status = 'Claimed',
                     claim_token = ?1,
                     claimed_by = ?2,
                     lease_until = ?3,
                     updated_at = ?4
                 WHERE job_id = ?5",
            )
            .map_err(|e| StorageError::Io(format!("prepare claim update: {e}")))?;

        for id in &job_ids {
            update_stmt
                .execute(params![
                    token,
                    consumer_id,
                    lease_until as i64,
                    now as i64,
                    id
                ])
                .map_err(|e| StorageError::Io(format!("claim update: {e}")))?;
        }

        // Re-read the claimed jobs.
        drop(update_stmt);
        drop(stmt);

        let mut result = Vec::with_capacity(job_ids.len());
        let mut load_stmt = conn
            .prepare_cached("SELECT * FROM mailbox_jobs WHERE job_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare claim reload: {e}")))?;

        for id in &job_ids {
            let job = load_stmt
                .query_row(params![id], row_to_job)
                .map_err(|e| StorageError::Io(format!("claim reload: {e}")))?;
            result.push(job);
        }

        Ok(result)
    }

    async fn claim_job(
        &self,
        job_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<MailboxJob>, StorageError> {
        let conn = self.conn.lock().await;

        // Check that the job exists and is Queued.
        let mut stmt = conn
            .prepare_cached("SELECT * FROM mailbox_jobs WHERE job_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare claim_job load: {e}")))?;

        let job = stmt
            .query_row(params![job_id], row_to_job)
            .optional()
            .map_err(|e| StorageError::Io(format!("claim_job load: {e}")))?;

        let job = match job {
            Some(j) if j.status == MailboxJobStatus::Queued => j,
            _ => return Ok(None),
        };

        // Same mailbox exclusivity: reject if another job for the same mailbox is Claimed.
        let has_other_claimed: bool = conn
            .prepare_cached(
                "SELECT EXISTS(
                    SELECT 1 FROM mailbox_jobs
                    WHERE mailbox_id = ?1
                      AND job_id != ?2
                      AND status = 'Claimed'
                )",
            )
            .map_err(|e| StorageError::Io(format!("prepare claim_job check: {e}")))?
            .query_row(params![job.mailbox_id, job_id], |row| row.get::<_, bool>(0))
            .map_err(|e| StorageError::Io(format!("claim_job check: {e}")))?;

        if has_other_claimed {
            return Ok(None);
        }

        let token = Uuid::now_v7().to_string();
        let lease_until = now + lease_ms;

        conn.execute(
            "UPDATE mailbox_jobs
             SET status = 'Claimed',
                 claim_token = ?1,
                 claimed_by = ?2,
                 lease_until = ?3,
                 updated_at = ?4
             WHERE job_id = ?5",
            params![token, consumer_id, lease_until as i64, now as i64, job_id],
        )
        .map_err(|e| StorageError::Io(format!("claim_job update: {e}")))?;

        // Re-read the updated job.
        drop(stmt);
        let updated = conn
            .prepare_cached("SELECT * FROM mailbox_jobs WHERE job_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare claim_job reload: {e}")))?
            .query_row(params![job_id], row_to_job)
            .map_err(|e| StorageError::Io(format!("claim_job reload: {e}")))?;

        Ok(Some(updated))
    }

    async fn ack(&self, job_id: &str, claim_token: &str, now: u64) -> Result<(), StorageError> {
        let conn = self.conn.lock().await;

        let job = conn
            .prepare_cached("SELECT * FROM mailbox_jobs WHERE job_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare ack load: {e}")))?
            .query_row(params![job_id], row_to_job)
            .optional()
            .map_err(|e| StorageError::Io(format!("ack load: {e}")))?
            .ok_or_else(|| StorageError::NotFound(job_id.to_string()))?;

        if job.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }

        conn.execute(
            "UPDATE mailbox_jobs
             SET status = 'Accepted', updated_at = ?1
             WHERE job_id = ?2",
            params![now as i64, job_id],
        )
        .map_err(|e| StorageError::Io(format!("ack update: {e}")))?;

        Ok(())
    }

    async fn nack(
        &self,
        job_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let conn = self.conn.lock().await;

        let job = conn
            .prepare_cached("SELECT * FROM mailbox_jobs WHERE job_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare nack load: {e}")))?
            .query_row(params![job_id], row_to_job)
            .optional()
            .map_err(|e| StorageError::Io(format!("nack load: {e}")))?
            .ok_or_else(|| StorageError::NotFound(job_id.to_string()))?;

        if job.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }

        let new_attempt_count = job.attempt_count + 1;

        if new_attempt_count >= job.max_attempts {
            // Dead letter.
            conn.execute(
                "UPDATE mailbox_jobs
                 SET status = 'DeadLetter',
                     attempt_count = ?1,
                     last_error = ?2,
                     updated_at = ?3
                 WHERE job_id = ?4",
                params![new_attempt_count as i64, error, now as i64, job_id],
            )
            .map_err(|e| StorageError::Io(format!("nack dead_letter update: {e}")))?;
        } else {
            // Requeue.
            conn.execute(
                "UPDATE mailbox_jobs
                 SET status = 'Queued',
                     attempt_count = ?1,
                     last_error = ?2,
                     available_at = ?3,
                     claim_token = NULL,
                     claimed_by = NULL,
                     lease_until = NULL,
                     updated_at = ?4
                 WHERE job_id = ?5",
                params![
                    new_attempt_count as i64,
                    error,
                    retry_at as i64,
                    now as i64,
                    job_id
                ],
            )
            .map_err(|e| StorageError::Io(format!("nack requeue update: {e}")))?;
        }

        Ok(())
    }

    async fn dead_letter(
        &self,
        job_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let conn = self.conn.lock().await;

        let job = conn
            .prepare_cached("SELECT * FROM mailbox_jobs WHERE job_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare dead_letter load: {e}")))?
            .query_row(params![job_id], row_to_job)
            .optional()
            .map_err(|e| StorageError::Io(format!("dead_letter load: {e}")))?
            .ok_or_else(|| StorageError::NotFound(job_id.to_string()))?;

        if job.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }

        conn.execute(
            "UPDATE mailbox_jobs
             SET status = 'DeadLetter',
                 last_error = ?1,
                 claim_token = NULL,
                 claimed_by = NULL,
                 lease_until = NULL,
                 updated_at = ?2
             WHERE job_id = ?3",
            params![error, now as i64, job_id],
        )
        .map_err(|e| StorageError::Io(format!("dead_letter update: {e}")))?;

        Ok(())
    }

    async fn cancel(&self, job_id: &str, now: u64) -> Result<Option<MailboxJob>, StorageError> {
        let conn = self.conn.lock().await;

        // Check that the job exists and is Queued.
        let job = conn
            .prepare_cached("SELECT * FROM mailbox_jobs WHERE job_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare cancel load: {e}")))?
            .query_row(params![job_id], row_to_job)
            .optional()
            .map_err(|e| StorageError::Io(format!("cancel load: {e}")))?;

        match job {
            Some(j) if j.status == MailboxJobStatus::Queued => {}
            _ => return Ok(None),
        }

        conn.execute(
            "UPDATE mailbox_jobs
             SET status = 'Cancelled', updated_at = ?1
             WHERE job_id = ?2",
            params![now as i64, job_id],
        )
        .map_err(|e| StorageError::Io(format!("cancel update: {e}")))?;

        let updated = conn
            .prepare_cached("SELECT * FROM mailbox_jobs WHERE job_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare cancel reload: {e}")))?
            .query_row(params![job_id], row_to_job)
            .map_err(|e| StorageError::Io(format!("cancel reload: {e}")))?;

        Ok(Some(updated))
    }

    async fn extend_lease(
        &self,
        job_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError> {
        let conn = self.conn.lock().await;

        let changed = conn
            .execute(
                "UPDATE mailbox_jobs
                 SET lease_until = ?1, updated_at = ?2
                 WHERE job_id = ?3
                   AND status = 'Claimed'
                   AND claim_token = ?4",
                params![(now + extension_ms) as i64, now as i64, job_id, claim_token],
            )
            .map_err(|e| StorageError::Io(format!("extend_lease update: {e}")))?;

        Ok(changed > 0)
    }

    async fn interrupt(
        &self,
        mailbox_id: &str,
        now: u64,
    ) -> Result<MailboxInterrupt, StorageError> {
        let conn = self.conn.lock().await;

        // Bump generation: INSERT ON CONFLICT DO UPDATE +1.
        conn.execute(
            "INSERT INTO mailbox_generations (mailbox_id, current_generation)
             VALUES (?1, 1)
             ON CONFLICT (mailbox_id) DO UPDATE
                SET current_generation = current_generation + 1",
            params![mailbox_id],
        )
        .map_err(|e| StorageError::Io(format!("interrupt bump gen: {e}")))?;

        let new_generation: i64 = conn
            .prepare_cached(
                "SELECT current_generation FROM mailbox_generations WHERE mailbox_id = ?1",
            )
            .map_err(|e| StorageError::Io(format!("prepare interrupt gen: {e}")))?
            .query_row(params![mailbox_id], |row| row.get(0))
            .map_err(|e| StorageError::Io(format!("interrupt gen select: {e}")))?;

        // Supersede all Queued jobs for this mailbox with generation < new_generation.
        let superseded_count = conn
            .execute(
                "UPDATE mailbox_jobs
                 SET status = 'Superseded', updated_at = ?1
                 WHERE mailbox_id = ?2
                   AND status = 'Queued'
                   AND generation < ?3",
                params![now as i64, mailbox_id, new_generation],
            )
            .map_err(|e| StorageError::Io(format!("interrupt supersede: {e}")))?;

        // Find active Claimed job if any.
        let active_job = conn
            .prepare_cached(
                "SELECT * FROM mailbox_jobs
                 WHERE mailbox_id = ?1 AND status = 'Claimed'
                 LIMIT 1",
            )
            .map_err(|e| StorageError::Io(format!("prepare interrupt active: {e}")))?
            .query_row(params![mailbox_id], row_to_job)
            .optional()
            .map_err(|e| StorageError::Io(format!("interrupt active: {e}")))?;

        Ok(MailboxInterrupt {
            new_generation: new_generation as u64,
            active_job,
            superseded_count,
        })
    }

    async fn load_job(&self, job_id: &str) -> Result<Option<MailboxJob>, StorageError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare_cached("SELECT * FROM mailbox_jobs WHERE job_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare load_job: {e}")))?;

        let result = stmt
            .query_row(params![job_id], row_to_job)
            .optional()
            .map_err(|e| StorageError::Io(format!("load_job: {e}")))?;

        Ok(result)
    }

    async fn list_jobs(
        &self,
        mailbox_id: &str,
        status_filter: Option<&[MailboxJobStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MailboxJob>, StorageError> {
        let conn = self.conn.lock().await;

        let (sql, dyn_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
            if let Some(statuses) = status_filter {
                if statuses.is_empty() {
                    return Ok(vec![]);
                }
                let placeholders: Vec<String> = statuses
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("?{}", i + 2))
                    .collect();
                let sql = format!(
                    "SELECT * FROM mailbox_jobs
                     WHERE mailbox_id = ?1 AND status IN ({})
                     ORDER BY priority ASC, created_at ASC
                     LIMIT {} OFFSET {}",
                    placeholders.join(","),
                    limit,
                    offset
                );
                let mut p: Vec<Box<dyn rusqlite::types::ToSql>> =
                    vec![Box::new(mailbox_id.to_string())];
                for s in statuses {
                    p.push(Box::new(status_to_str(*s).to_string()));
                }
                (sql, p)
            } else {
                let sql = format!(
                    "SELECT * FROM mailbox_jobs
                     WHERE mailbox_id = ?1
                     ORDER BY priority ASC, created_at ASC
                     LIMIT {} OFFSET {}",
                    limit, offset
                );
                let p: Vec<Box<dyn rusqlite::types::ToSql>> =
                    vec![Box::new(mailbox_id.to_string())];
                (sql, p)
            };

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            dyn_params.iter().map(|b| b.as_ref()).collect();

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| StorageError::Io(format!("prepare list_jobs: {e}")))?;

        let rows = stmt
            .query_map(param_refs.as_slice(), row_to_job)
            .map_err(|e| StorageError::Io(format!("list_jobs query: {e}")))?;

        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row.map_err(|e| StorageError::Io(format!("list_jobs row: {e}")))?);
        }
        Ok(jobs)
    }

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<MailboxJob>, StorageError> {
        let conn = self.conn.lock().await;

        // Step 1: Find expired Claimed jobs.
        let mut stmt = conn
            .prepare_cached(
                "SELECT job_id, attempt_count, max_attempts FROM mailbox_jobs
                 WHERE status = 'Claimed'
                   AND lease_until < ?1
                 LIMIT ?2",
            )
            .map_err(|e| StorageError::Io(format!("prepare reclaim select: {e}")))?;

        let expired: Vec<(String, u32, u32)> = stmt
            .query_map(params![now as i64, limit as i64], |row| {
                let id: String = row.get(0)?;
                let attempt: i64 = row.get(1)?;
                let max: i64 = row.get(2)?;
                Ok((id, attempt as u32, max as u32))
            })
            .map_err(|e| StorageError::Io(format!("reclaim select: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Io(format!("reclaim collect: {e}")))?;

        if expired.is_empty() {
            return Ok(vec![]);
        }

        // Step 2: Update each expired job.
        let mut requeue_stmt = conn
            .prepare_cached(
                "UPDATE mailbox_jobs
                 SET status = 'Queued',
                     attempt_count = ?1,
                     claim_token = NULL,
                     claimed_by = NULL,
                     lease_until = NULL,
                     updated_at = ?2
                 WHERE job_id = ?3",
            )
            .map_err(|e| StorageError::Io(format!("prepare reclaim requeue: {e}")))?;

        let mut deadletter_stmt = conn
            .prepare_cached(
                "UPDATE mailbox_jobs
                 SET status = 'DeadLetter',
                     attempt_count = ?1,
                     updated_at = ?2
                 WHERE job_id = ?3",
            )
            .map_err(|e| StorageError::Io(format!("prepare reclaim deadletter: {e}")))?;

        for (id, attempt_count, max_attempts) in &expired {
            let new_attempt = attempt_count + 1;
            if new_attempt >= *max_attempts {
                deadletter_stmt
                    .execute(params![new_attempt as i64, now as i64, id])
                    .map_err(|e| StorageError::Io(format!("reclaim deadletter: {e}")))?;
            } else {
                requeue_stmt
                    .execute(params![new_attempt as i64, now as i64, id])
                    .map_err(|e| StorageError::Io(format!("reclaim requeue: {e}")))?;
            }
        }

        // Step 3: Re-read the updated jobs.
        drop(requeue_stmt);
        drop(deadletter_stmt);
        drop(stmt);

        let mut result = Vec::with_capacity(expired.len());
        let mut load_stmt = conn
            .prepare_cached("SELECT * FROM mailbox_jobs WHERE job_id = ?1")
            .map_err(|e| StorageError::Io(format!("prepare reclaim reload: {e}")))?;

        for (id, _, _) in &expired {
            let job = load_stmt
                .query_row(params![id], row_to_job)
                .map_err(|e| StorageError::Io(format!("reclaim reload: {e}")))?;
            result.push(job);
        }

        Ok(result)
    }

    async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError> {
        let conn = self.conn.lock().await;

        let deleted = conn
            .execute(
                "DELETE FROM mailbox_jobs
                 WHERE status IN ('Accepted', 'Cancelled', 'Superseded', 'DeadLetter')
                   AND updated_at < ?1",
                params![older_than as i64],
            )
            .map_err(|e| StorageError::Io(format!("purge_terminal: {e}")))?;

        Ok(deleted)
    }

    async fn queued_mailbox_ids(&self) -> Result<Vec<String>, StorageError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare_cached(
                "SELECT DISTINCT mailbox_id FROM mailbox_jobs
                 WHERE status = 'Queued'
                 ORDER BY mailbox_id",
            )
            .map_err(|e| StorageError::Io(format!("prepare queued_mailbox_ids: {e}")))?;

        let ids: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| StorageError::Io(format!("queued_mailbox_ids query: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::Io(format!("queued_mailbox_ids collect: {e}")))?;

        Ok(ids)
    }
}

// We need the `optional()` extension on `Result<T, rusqlite::Error>`.
trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::message::Message;

    fn make_job(id: &str, mailbox: &str) -> MailboxJob {
        MailboxJob {
            job_id: id.to_string(),
            mailbox_id: mailbox.to_string(),
            agent_id: "agent-1".to_string(),
            messages: vec![Message::user("hello")],
            origin: MailboxJobOrigin::User,
            sender_id: None,
            parent_run_id: None,
            request_extras: None,
            priority: 128,
            dedupe_key: None,
            generation: 0,
            status: MailboxJobStatus::Queued,
            available_at: 0,
            attempt_count: 0,
            max_attempts: 5,
            last_error: None,
            claim_token: None,
            claimed_by: None,
            lease_until: None,
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[tokio::test]
    async fn enqueue_and_load() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let job = make_job("job-1", "mbox-a");

        store.enqueue(&job).await.unwrap();

        let loaded = store.load_job("job-1").await.unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.job_id, "job-1");
        assert_eq!(loaded.mailbox_id, "mbox-a");
        assert_eq!(loaded.agent_id, "agent-1");
        assert_eq!(loaded.status, MailboxJobStatus::Queued);
        assert_eq!(loaded.generation, 0);
        assert_eq!(loaded.priority, 128);
        assert_eq!(loaded.messages.len(), 1);

        // Non-existent job returns None.
        let missing = store.load_job("no-such-job").await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn enqueue_dedupe_rejects_duplicate() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        let mut job1 = make_job("job-1", "mbox-a");
        job1.dedupe_key = Some("dk-1".to_string());
        store.enqueue(&job1).await.unwrap();

        // Second enqueue with same dedupe_key should fail.
        let mut job2 = make_job("job-2", "mbox-a");
        job2.dedupe_key = Some("dk-1".to_string());
        let result = store.enqueue(&job2).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            StorageError::AlreadyExists(msg) => assert!(msg.contains("dk-1")),
            other => panic!("expected AlreadyExists, got: {other:?}"),
        }

        // Different dedupe_key should succeed.
        let mut job3 = make_job("job-3", "mbox-a");
        job3.dedupe_key = Some("dk-2".to_string());
        store.enqueue(&job3).await.unwrap();

        // Same dedupe_key in a different mailbox should succeed.
        let mut job4 = make_job("job-4", "mbox-b");
        job4.dedupe_key = Some("dk-1".to_string());
        store.enqueue(&job4).await.unwrap();
    }

    #[tokio::test]
    async fn list_jobs_filters_by_status() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        // Enqueue 3 jobs for the same mailbox.
        for i in 0..3 {
            let job = make_job(&format!("job-{i}"), "mbox-a");
            store.enqueue(&job).await.unwrap();
        }

        // Also enqueue one for a different mailbox (should not appear).
        let other = make_job("job-other", "mbox-b");
        store.enqueue(&other).await.unwrap();

        // List all jobs for mbox-a (no status filter).
        let all = store.list_jobs("mbox-a", None, 100, 0).await.unwrap();
        assert_eq!(all.len(), 3);

        // Filter by Queued status.
        let queued = store
            .list_jobs("mbox-a", Some(&[MailboxJobStatus::Queued]), 100, 0)
            .await
            .unwrap();
        assert_eq!(queued.len(), 3);

        // Filter by Claimed (none exist).
        let claimed = store
            .list_jobs("mbox-a", Some(&[MailboxJobStatus::Claimed]), 100, 0)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 0);

        // Test limit.
        let limited = store.list_jobs("mbox-a", None, 2, 0).await.unwrap();
        assert_eq!(limited.len(), 2);

        // Test offset.
        let offset = store.list_jobs("mbox-a", None, 100, 2).await.unwrap();
        assert_eq!(offset.len(), 1);
    }

    #[tokio::test]
    async fn list_jobs_sorted_by_priority_then_created_at() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        let mut j1 = make_job("job-low", "mbox-a");
        j1.priority = 200;
        j1.created_at = 100;
        store.enqueue(&j1).await.unwrap();

        let mut j2 = make_job("job-high", "mbox-a");
        j2.priority = 10;
        j2.created_at = 200;
        store.enqueue(&j2).await.unwrap();

        let mut j3 = make_job("job-high-early", "mbox-a");
        j3.priority = 10;
        j3.created_at = 50;
        store.enqueue(&j3).await.unwrap();

        let list = store.list_jobs("mbox-a", None, 100, 0).await.unwrap();
        assert_eq!(list.len(), 3);
        // priority 10 created_at 50
        assert_eq!(list[0].job_id, "job-high-early");
        // priority 10 created_at 200
        assert_eq!(list[1].job_id, "job-high");
        // priority 200
        assert_eq!(list[2].job_id, "job-low");
    }

    #[tokio::test]
    async fn enqueue_sets_generation_from_store() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        let mut job = make_job("job-1", "mbox-a");
        job.generation = 999; // should be overridden
        store.enqueue(&job).await.unwrap();

        let loaded = store.load_job("job-1").await.unwrap().unwrap();
        assert_eq!(
            loaded.generation, 0,
            "generation should come from store, not from input"
        );
    }

    #[tokio::test]
    async fn enqueue_preserves_request_extras() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        let mut job = make_job("job-extras", "mbox-a");
        job.request_extras = Some(serde_json::json!({"temperature": 0.7, "max_tokens": 1000}));
        store.enqueue(&job).await.unwrap();

        let loaded = store.load_job("job-extras").await.unwrap().unwrap();
        let extras = loaded.request_extras.unwrap();
        assert_eq!(extras["temperature"], 0.7);
        assert_eq!(extras["max_tokens"], 1000);
    }

    #[tokio::test]
    async fn claim_and_ack() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let job = make_job("job-1", "mbox-a");
        store.enqueue(&job).await.unwrap();

        // Claim the job.
        let claimed = store
            .claim("mbox-a", "consumer-1", 30_000, 2000, 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].job_id, "job-1");
        assert_eq!(claimed[0].status, MailboxJobStatus::Claimed);
        assert!(claimed[0].claim_token.is_some());
        assert_eq!(claimed[0].claimed_by.as_deref(), Some("consumer-1"));
        assert_eq!(claimed[0].lease_until, Some(32_000));

        // Cannot double-claim while a job is Claimed in this mailbox.
        let mut job2 = make_job("job-2", "mbox-a");
        job2.created_at = 2000;
        job2.updated_at = 2000;
        store.enqueue(&job2).await.unwrap();
        let double = store
            .claim("mbox-a", "consumer-2", 30_000, 2000, 10)
            .await
            .unwrap();
        assert!(
            double.is_empty(),
            "should not claim while another is Claimed"
        );

        // Ack the first job.
        let token = claimed[0].claim_token.as_ref().unwrap();
        store.ack("job-1", token, 3000).await.unwrap();

        let loaded = store.load_job("job-1").await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::Accepted);
    }

    #[tokio::test]
    async fn nack_increments_attempt_and_requeues() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let mut job = make_job("job-1", "mbox-a");
        job.max_attempts = 3;
        store.enqueue(&job).await.unwrap();

        // Claim then nack.
        let claimed = store.claim("mbox-a", "c1", 30_000, 1000, 10).await.unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap();

        store
            .nack("job-1", token, 5000, "transient error", 2000)
            .await
            .unwrap();

        let loaded = store.load_job("job-1").await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::Queued);
        assert_eq!(loaded.attempt_count, 1);
        assert_eq!(loaded.available_at, 5000);
        assert_eq!(loaded.last_error.as_deref(), Some("transient error"));
        assert!(loaded.claim_token.is_none());
        assert!(loaded.claimed_by.is_none());
        assert!(loaded.lease_until.is_none());
    }

    #[tokio::test]
    async fn nack_dead_letters_on_max_attempts() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let mut job = make_job("job-1", "mbox-a");
        job.max_attempts = 1;
        store.enqueue(&job).await.unwrap();

        let claimed = store.claim("mbox-a", "c1", 30_000, 1000, 10).await.unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap();

        store
            .nack("job-1", token, 5000, "fatal", 2000)
            .await
            .unwrap();

        let loaded = store.load_job("job-1").await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::DeadLetter);
        assert_eq!(loaded.attempt_count, 1);
        assert_eq!(loaded.last_error.as_deref(), Some("fatal"));
    }

    #[tokio::test]
    async fn dead_letter_explicit() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let job = make_job("job-1", "mbox-a");
        store.enqueue(&job).await.unwrap();

        let claimed = store.claim("mbox-a", "c1", 30_000, 1000, 10).await.unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap();

        store
            .dead_letter("job-1", token, "permanent failure", 2000)
            .await
            .unwrap();

        let loaded = store.load_job("job-1").await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::DeadLetter);
        assert_eq!(loaded.last_error.as_deref(), Some("permanent failure"));
        assert!(loaded.claim_token.is_none());
        assert!(loaded.claimed_by.is_none());
        assert!(loaded.lease_until.is_none());
    }

    #[tokio::test]
    async fn ack_wrong_token_fails() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let job = make_job("job-1", "mbox-a");
        store.enqueue(&job).await.unwrap();

        let claimed = store.claim("mbox-a", "c1", 30_000, 1000, 10).await.unwrap();
        assert_eq!(claimed.len(), 1);

        let result = store.ack("job-1", "wrong-token", 2000).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            StorageError::VersionConflict { .. } => {}
            other => panic!("expected VersionConflict, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_queued_job() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let job = make_job("job-1", "mbox-a");
        store.enqueue(&job).await.unwrap();

        let cancelled = store.cancel("job-1", 2000).await.unwrap();
        assert!(cancelled.is_some());
        let cancelled = cancelled.unwrap();
        assert_eq!(cancelled.status, MailboxJobStatus::Cancelled);
        assert_eq!(cancelled.updated_at, 2000);

        // Verify persisted.
        let loaded = store.load_job("job-1").await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::Cancelled);

        // Cancel a non-Queued job returns None.
        let again = store.cancel("job-1", 3000).await.unwrap();
        assert!(again.is_none());

        // Cancel non-existent job returns None.
        let missing = store.cancel("no-such-job", 3000).await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn interrupt_supersedes_queued() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        store.enqueue(&make_job("job-1", "mbox-a")).await.unwrap();
        store.enqueue(&make_job("job-2", "mbox-a")).await.unwrap();

        let result = store.interrupt("mbox-a", 2000).await.unwrap();
        assert_eq!(result.new_generation, 1);
        assert_eq!(result.superseded_count, 2);
        assert!(result.active_job.is_none());

        // Verify jobs are Superseded.
        let listed = store
            .list_jobs("mbox-a", Some(&[MailboxJobStatus::Superseded]), 100, 0)
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn interrupt_returns_active_claimed_job() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let job1 = make_job("job-1", "mbox-a");
        store.enqueue(&job1).await.unwrap();

        // Claim the first job.
        let claimed = store
            .claim("mbox-a", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);

        // Enqueue a second job (Queued).
        store.enqueue(&make_job("job-2", "mbox-a")).await.unwrap();

        let result = store.interrupt("mbox-a", 2000).await.unwrap();
        assert_eq!(result.new_generation, 1);
        assert_eq!(result.superseded_count, 1); // only job-2 was Queued
        assert!(result.active_job.is_some());
        assert_eq!(result.active_job.unwrap().job_id, "job-1");
    }

    #[tokio::test]
    async fn extend_lease_succeeds() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let job = make_job("job-1", "mbox-a");
        store.enqueue(&job).await.unwrap();

        let claimed = store
            .claim("mbox-a", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        let ok = store
            .extend_lease("job-1", &token, 60_000, 15_000)
            .await
            .unwrap();
        assert!(ok);

        let loaded = store.load_job("job-1").await.unwrap().unwrap();
        assert_eq!(loaded.lease_until, Some(75_000));

        // Wrong token returns false.
        let nope = store
            .extend_lease("job-1", "wrong-token", 60_000, 20_000)
            .await
            .unwrap();
        assert!(!nope);

        // Non-existent job returns false.
        let nope2 = store
            .extend_lease("no-such-job", &token, 60_000, 20_000)
            .await
            .unwrap();
        assert!(!nope2);
    }

    #[tokio::test]
    async fn reclaim_expired_leases() {
        let store = SqliteMailboxStore::open_memory().unwrap();
        let job = make_job("job-1", "mbox-a");
        store.enqueue(&job).await.unwrap();

        // Claim with a short lease.
        let claimed = store
            .claim("mbox-a", "consumer-1", 1_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed[0].lease_until, Some(2_000));

        // At now=3000, lease is expired.
        let reclaimed = store.reclaim_expired_leases(3000, 10).await.unwrap();
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].job_id, "job-1");
        assert_eq!(reclaimed[0].status, MailboxJobStatus::Queued);
        assert_eq!(reclaimed[0].attempt_count, 1);
        assert!(reclaimed[0].claim_token.is_none());
        assert!(reclaimed[0].claimed_by.is_none());
        assert!(reclaimed[0].lease_until.is_none());

        // Not expired yet: no reclaims.
        let claimed2 = store
            .claim("mbox-a", "consumer-2", 100_000, 4000, 1)
            .await
            .unwrap();
        assert_eq!(claimed2.len(), 1);
        let none = store.reclaim_expired_leases(5000, 10).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn purge_terminal_removes_old() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        // Create and cancel a job (terminal).
        let job1 = make_job("job-1", "mbox-a");
        store.enqueue(&job1).await.unwrap();
        store.cancel("job-1", 1000).await.unwrap();

        // Create a Queued job (non-terminal, should not be purged).
        let job2 = make_job("job-2", "mbox-a");
        store.enqueue(&job2).await.unwrap();

        // Purge with threshold after the cancelled job's updated_at.
        let purged = store.purge_terminal(2000).await.unwrap();
        assert_eq!(purged, 1);

        // The cancelled job is gone.
        let loaded = store.load_job("job-1").await.unwrap();
        assert!(loaded.is_none());

        // The queued job remains.
        let loaded2 = store.load_job("job-2").await.unwrap();
        assert!(loaded2.is_some());
    }

    #[tokio::test]
    async fn queued_mailbox_ids() {
        let store = SqliteMailboxStore::open_memory().unwrap();

        // No jobs yet.
        let ids = store.queued_mailbox_ids().await.unwrap();
        assert!(ids.is_empty());

        // Add queued jobs in two mailboxes.
        store.enqueue(&make_job("job-1", "mbox-b")).await.unwrap();
        store.enqueue(&make_job("job-2", "mbox-a")).await.unwrap();
        store.enqueue(&make_job("job-3", "mbox-a")).await.unwrap();

        let ids = store.queued_mailbox_ids().await.unwrap();
        assert_eq!(ids, vec!["mbox-a", "mbox-b"]);

        // Cancel all jobs in mbox-a.
        store.cancel("job-2", 2000).await.unwrap();
        store.cancel("job-3", 2000).await.unwrap();

        let ids = store.queued_mailbox_ids().await.unwrap();
        assert_eq!(ids, vec!["mbox-b"]);
    }
}
