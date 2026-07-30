//! Durable [`ManagedSessionRepository`] backends: the home for the Managed
//! session aggregate (agent / model / title / metadata / accepted MCP servers),
//! over the store's own `managed` migration scope ([`session_bundle`]). Two
//! backends — [`SqliteManagedSessionRepository`] (embedded, `sessions.db`) and
//! [`PostgresManagedSessionRepository`] (network DB) — share the one portable
//! bundle, exactly like the config/catalog/credential stores.
//!
//! It is its OWN scope (`managed_session` table + `managed_schema_migrations`
//! ledger), NOT a table in the authoring-plane `admin.db`: a live session
//! instance is a different aggregate from the agent/MCP *definitions* admin holds,
//! so mixing them would cross a bounded-context line (ADR-0039 "one repository per
//! aggregate"). Secrets never land here — only the wire-echo MCP `{name,type,url}`
//! values, per the port's contract (G3).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, ManagedSessionRepository, PersistedSession,
    ScopedPersistedSession, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionRepositoryError, SessionRevision,
};

mod deployments;
mod dream;
mod extraction;
mod row_codec;
use row_codec::{EncodedSessionRow, decode};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sqlx::Row;
use sqlx::postgres::PgPool;

/// The session store's table namespace / bundle prefix: the table is
/// `managed_session`, the ledger `managed_schema_migrations`.
const NS: &str = "managed";
const SQLITE_WRITE_WAIT: Duration = Duration::from_secs(30);

/// The versioned schema bundle (ADR-0043 scoped migration). One migration: the
/// `managed_session` row. All columns are portable — the JSON payloads live in
/// `TEXT` columns as serde strings on both backends, so the two adapters read and
/// write identical rows.
fn session_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.managed_session",
        vec![
            Migration::new(
                1,
                "managed session config: one row per session id (secret-free)",
                "CREATE TABLE {prefix}_session (\
                 session_id     TEXT PRIMARY KEY, \
                 agent_id       TEXT NOT NULL, \
                 model          TEXT NOT NULL, \
                 title          TEXT, \
                 metadata_json  TEXT NOT NULL, \
                 environment_id TEXT NOT NULL, \
                 mcp_json       TEXT NOT NULL)",
            )?,
            // Tenancy edge aspect (ADR-0051): the opaque owner `scope_id`, so the
            // edge ownership guard can fence a cross-tenant request across a
            // restart. Additive with a seeded default; the config columns stay
            // tenancy-agnostic — this is a separate ownership fact, not part of the
            // aggregate.
            Migration::new(
                2,
                "managed session owner scope_id (ADR-0051)",
                "ALTER TABLE {prefix}_session ADD COLUMN scope_id TEXT NOT NULL DEFAULT 'default'",
            )?,
            Migration::new(
                3,
                "session lifecycle transactional outbox",
                "CREATE TABLE {prefix}_lifecycle_outbox (\
                    fact_id TEXT PRIMARY KEY, \
                    data TEXT NOT NULL, \
                    created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            Migration::new(
                4,
                "managed session durable lifecycle status",
                "ALTER TABLE {prefix}_session ADD COLUMN status TEXT NOT NULL DEFAULT 'idle'",
            )?,
            Migration::new(
                5,
                "managed session durable archive timestamp",
                "ALTER TABLE {prefix}_session ADD COLUMN archived_at TEXT",
            )?,
            Migration::new(
                6,
                "managed session frozen effective resource inputs",
                "ALTER TABLE {prefix}_session ADD COLUMN effective_inputs_json TEXT NOT NULL DEFAULT '{\"inputs\":[]}'",
            )?,
            Migration::new(
                7,
                "durable Memory extraction intents",
                "CREATE TABLE {prefix}_memory_extraction (\
                    intent_id TEXT PRIMARY KEY, \
                    idempotency_key TEXT NOT NULL UNIQUE, \
                    status TEXT NOT NULL, \
                    revision BIGINT NOT NULL, \
                    lease_expires_at_unix_ms BIGINT, \
                    data TEXT NOT NULL, \
                    created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            Migration::new(
                8,
                "managed session runtime environment binding",
                "ALTER TABLE {prefix}_session ADD COLUMN environment_binding TEXT",
            )?,
            Migration::new(
                9,
                "managed session secret-free runtime initialization pin",
                "ALTER TABLE {prefix}_session ADD COLUMN runtime_json TEXT NOT NULL DEFAULT '{\"mcp_servers\":[],\"runtime\":null,\"deny_egress\":false,\"sandbox\":null}'",
            )?,
            Migration::new(
                10,
                "managed session root optimistic-concurrency revision",
                "ALTER TABLE {prefix}_session ADD COLUMN revision BIGINT NOT NULL DEFAULT 1",
            )?,
            Migration::new(
                11,
                "managed session idempotency receipts",
                "CREATE TABLE {prefix}_session_idempotency (\
                    session_id TEXT NOT NULL, \
                    idempotency_key TEXT NOT NULL, \
                    payload_hash TEXT NOT NULL, \
                    committed_revision BIGINT NOT NULL, \
                    PRIMARY KEY (session_id, idempotency_key))",
            )?,
            Migration::new(
                12,
                "managed session durable delete tombstones",
                "CREATE TABLE {prefix}_session_tombstone (\
                    session_id TEXT PRIMARY KEY, \
                    scope_id TEXT NOT NULL, \
                    deleted_revision BIGINT NOT NULL, \
                    deleted_at TEXT NOT NULL)",
            )?,
            Migration::new(
                13,
                "one canonical serialized Session aggregate (ADR-0066)",
                "ALTER TABLE {prefix}_session ADD COLUMN aggregate_json TEXT",
            )?,
            Migration::new(
                14,
                "durable Dream jobs",
                "CREATE TABLE {prefix}_dream (\
                    job_id TEXT PRIMARY KEY, \
                    data TEXT NOT NULL)",
            )?,
            Migration::new(
                15,
                "Workspace Dream Agent overrides",
                "CREATE TABLE {prefix}_dream_agent_override (\
                    workspace_id TEXT PRIMARY KEY, \
                    agent_id TEXT NOT NULL)",
            )?,
            Migration::new(
                16,
                "durable Managed Deployments",
                "CREATE TABLE {prefix}_deployment (\
                    deployment_id TEXT PRIMARY KEY, \
                    workspace_id TEXT NOT NULL, \
                    data TEXT NOT NULL)",
            )?,
            Migration::new(
                17,
                "durable Managed DeploymentRuns",
                "CREATE TABLE {prefix}_deployment_run (\
                    run_id TEXT PRIMARY KEY, \
                    deployment_id TEXT NOT NULL, \
                    workspace_id TEXT NOT NULL, \
                    data TEXT NOT NULL)",
            )?,
            Migration::new(
                18,
                "exactly-once scheduled Deployment occurrence claims",
                "CREATE TABLE {prefix}_deployment_claim (\
                    claim_id TEXT PRIMARY KEY, \
                    run_id TEXT NOT NULL UNIQUE, \
                    created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            Migration::new(
                19,
                "Workspace Dream scheduling policies",
                "CREATE TABLE {prefix}_dream_policy (\
                    workspace_id TEXT NOT NULL, \
                    memory_store_id TEXT NOT NULL, \
                    data TEXT NOT NULL, \
                    PRIMARY KEY (workspace_id, memory_store_id))",
            )?,
        ],
    )
}

fn aggregate_str(session: &PersistedSession) -> String {
    serde_json::to_string(session).expect("Session aggregate serializes")
}

fn lifecycle_str(fact: &ManagedLifecycleFact) -> String {
    serde_json::json!({
        "id": fact.id,
        "object_id": fact.object_id,
        "workspace_id": fact.workspace_id,
        "event_type": fact.event_type,
        "timestamp": fact.timestamp,
    })
    .to_string()
}

fn db_revision(revision: SessionRevision) -> Result<i64, SessionRepositoryError> {
    i64::try_from(revision.0).map_err(|_| {
        SessionRepositoryError::InvalidMutation("Session revision exceeds i64 storage".into())
    })
}

fn storage(error: impl std::fmt::Display) -> SessionRepositoryError {
    SessionRepositoryError::Storage(error.to_string())
}

fn decode_lifecycle(data: &str) -> Result<ManagedLifecycleFact, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(data)?;
    Ok(ManagedLifecycleFact {
        id: value["id"].as_str().unwrap_or_default().to_string(),
        object_id: value["object_id"]
            .as_str()
            .or_else(|| value["session_id"].as_str())
            .unwrap_or_default()
            .to_string(),
        workspace_id: value["workspace_id"].as_str().map(str::to_string),
        event_type: value["event_type"].as_str().unwrap_or_default().to_string(),
        timestamp: value["timestamp"].as_i64().unwrap_or_default(),
    })
}

/// SQLite persistence for [`PersistedSession`]. One row per session, keyed by id.
pub struct SqliteManagedSessionRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteManagedSessionRepository {
    /// Open (or create) `sessions.db` at `path` and apply the schema migrations.
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    /// An in-memory database (tests).
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, String> {
        // Embedded compositions intentionally colocate several aggregate stores
        // in one WAL database. A busy timeout is connection-local, so the
        // bootstrap connection cannot configure this repository's connection.
        conn.busy_timeout(SQLITE_WRITE_WAIT)
            .map_err(|e| e.to_string())?;
        let bundle = session_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&conn, &bundle)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

#[async_trait]
impl ManagedSessionRepository for SqliteManagedSessionRepository {
    async fn create(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionRevision, SessionRepositoryError> {
        if session.session_id.trim().is_empty()
            || idempotency.key.trim().is_empty()
            || idempotency.payload_hash.trim().is_empty()
            || session.revision != SessionRevision(0)
            || lifecycle_facts
                .iter()
                .any(|fact| fact.object_id != session.session_id)
        {
            return Err(SessionRepositoryError::InvalidMutation(
                "invalid Session create command".into(),
            ));
        }
        let mut conn = self.conn.lock().map_err(storage)?;
        // Acquire the SQLite writer reservation before reading the expected
        // revision. A deferred read-then-write transaction can otherwise fail
        // its upgrade immediately when another aggregate writes concurrently.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        if let Some((stored_hash, committed_revision)) = tx
            .query_row(
                "SELECT payload_hash, committed_revision FROM managed_session_idempotency
                 WHERE session_id = ?1 AND idempotency_key = ?2",
                params![session.session_id, idempotency.key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(storage)?
        {
            if stored_hash != idempotency.payload_hash {
                return Err(SessionRepositoryError::IdempotencyMismatch);
            }
            return u64::try_from(committed_revision)
                .map(SessionRevision)
                .map_err(|_| storage("negative committed Session revision"));
        }
        let tombstoned = tx
            .query_row(
                "SELECT 1 FROM managed_session_tombstone WHERE session_id = ?1",
                params![session.session_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage)?
            .is_some();
        if tombstoned {
            return Err(SessionRepositoryError::Tombstoned);
        }
        let new_revision = SessionRevision(1);
        session.revision = new_revision;
        let inserted = tx
            .execute(
                r#"INSERT OR IGNORE INTO managed_session
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json,
                 scope_id, status, archived_at, effective_inputs_json, environment_binding,
                 runtime_json, revision, aggregate_json)
             VALUES (?1, '', '', NULL, '{}', '', '[]', ?2, 'aggregate', NULL,
                     '{"inputs":[]}', NULL,
                     '{"mcp_servers":[],"runtime":null,"deny_egress":false,"sandbox":null}',
                     ?3, ?4)"#,
                params![
                    session.session_id,
                    owner_scope,
                    db_revision(new_revision)?,
                    aggregate_str(&session),
                ],
            )
            .map_err(storage)?;
        if inserted != 1 {
            return Err(SessionRepositoryError::AlreadyExists);
        }
        tx.execute(
            "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                session.session_id,
                idempotency.key,
                idempotency.payload_hash,
                db_revision(new_revision)?,
            ],
        )
        .map_err(storage)?;
        for fact in lifecycle_facts {
            tx.execute(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
                params![fact.id, lifecycle_str(&fact)],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok(new_revision)
    }

    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        let next = mutation
            .validate()
            .map_err(|error| SessionRepositoryError::InvalidMutation(error.to_string()))?;
        let session_id = mutation.payload.session_id().to_string();
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        if let Some((stored_hash, committed_revision)) = tx
            .query_row(
                "SELECT payload_hash, committed_revision FROM managed_session_idempotency
                 WHERE session_id = ?1 AND idempotency_key = ?2",
                params![session_id, mutation.idempotency.key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(storage)?
        {
            if stored_hash != mutation.idempotency.payload_hash {
                return Ok(SessionMutationResult::IdempotencyMismatch);
            }
            let committed_revision = SessionRevision(
                u64::try_from(committed_revision)
                    .map_err(|_| storage("negative committed Session revision"))?,
            );
            return Ok(SessionMutationResult::Replayed {
                new_revision: committed_revision,
            });
        }
        let current = tx
            .query_row(
                "SELECT revision, scope_id FROM managed_session WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(storage)?;
        let Some((current_revision, current_owner)) = current else {
            let tombstone_revision = tx
                .query_row(
                    "SELECT deleted_revision FROM managed_session_tombstone WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(storage)?
                .unwrap_or_default();
            return Ok(SessionMutationResult::Conflict {
                current_revision: SessionRevision(
                    u64::try_from(tombstone_revision).unwrap_or_default(),
                ),
            });
        };
        let current_revision = SessionRevision(
            u64::try_from(current_revision)
                .map_err(|_| storage("negative managed Session revision"))?,
        );
        if current_owner != owner_scope || current_revision != mutation.expected_revision {
            return Ok(SessionMutationResult::Conflict { current_revision });
        }
        match &mutation.payload {
            SessionMutationPayload::Replace(replacement) => {
                let mut replacement = replacement.clone();
                replacement.revision = next;
                let affected = tx
                    .execute(
                        "UPDATE managed_session SET
                        aggregate_json = ?2, revision = ?3
                     WHERE session_id = ?1 AND scope_id = ?4 AND revision = ?5",
                        params![
                            replacement.session_id,
                            aggregate_str(&replacement),
                            db_revision(next)?,
                            owner_scope,
                            db_revision(current_revision)?,
                        ],
                    )
                    .map_err(storage)?;
                if affected != 1 {
                    return Ok(SessionMutationResult::Conflict { current_revision });
                }
            }
            SessionMutationPayload::Delete(tombstone) => {
                let affected = tx
                    .execute(
                        "DELETE FROM managed_session
                         WHERE session_id = ?1 AND scope_id = ?2 AND revision = ?3",
                        params![session_id, owner_scope, db_revision(current_revision)?],
                    )
                    .map_err(storage)?;
                if affected != 1 {
                    return Ok(SessionMutationResult::Conflict { current_revision });
                }
                tx.execute(
                    "INSERT INTO managed_session_tombstone
                        (session_id, scope_id, deleted_revision, deleted_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        tombstone.session_id,
                        owner_scope,
                        db_revision(tombstone.deleted_revision)?,
                        tombstone.deleted_at,
                    ],
                )
                .map_err(storage)?;
            }
        }
        tx.execute(
            "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                session_id,
                mutation.idempotency.key,
                mutation.idempotency.payload_hash,
                db_revision(next)?,
            ],
        )
        .map_err(storage)?;
        for fact in mutation.lifecycle_facts {
            tx.execute(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
                params![fact.id, lifecycle_str(&fact)],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok(SessionMutationResult::Applied { new_revision: next })
    }

    async fn append_lifecycle(&self, fact: ManagedLifecycleFact) {
        let data = lifecycle_str(&fact);
        self.conn
            .lock()
            .expect("session store mutex poisoned")
            .execute(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
                params![fact.id, data],
            )
            .expect("append session lifecycle fact");
    }

    async fn pending_lifecycle(&self) -> Vec<ManagedLifecycleFact> {
        let conn = self.conn.lock().expect("session store mutex poisoned");
        let mut statement = conn
            .prepare("SELECT data FROM managed_lifecycle_outbox ORDER BY created_at, fact_id")
            .expect("prepare pending lifecycle facts");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query pending lifecycle facts")
            .map(|row| {
                decode_lifecycle(&row.expect("read lifecycle fact")).expect("decode lifecycle fact")
            })
            .collect()
    }

    async fn complete_lifecycle(&self, fact_id: &str) {
        self.conn
            .lock()
            .expect("session store mutex poisoned")
            .execute(
                "DELETE FROM managed_lifecycle_outbox WHERE fact_id = ?1",
                params![fact_id],
            )
            .expect("complete lifecycle fact");
    }

    async fn get(&self, session_id: &str) -> Option<PersistedSession> {
        let conn = self.conn.lock().expect("session store mutex poisoned");
        let raw = conn
            .query_row(
                "SELECT aggregate_json, agent_id, model, title, metadata_json, environment_id, status, archived_at, effective_inputs_json, environment_binding, runtime_json, revision
                 FROM managed_session WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, i64>(11)?,
                    ))
                },
            )
            .optional()
            .expect("read managed session")?;
        let (
            aggregate_json,
            agent_id,
            model,
            title,
            metadata_json,
            environment_id,
            status,
            archived_at,
            effective_inputs_json,
            environment_binding,
            runtime_json,
            revision,
        ) = raw;
        Some(
            decode(EncodedSessionRow {
                aggregate_json,
                session_id: session_id.to_string(),
                agent_id,
                model,
                title,
                metadata_json,
                environment_id,
                effective_inputs_json,
                environment_binding,
                runtime_json,
                status,
                archived_at,
                revision,
            })
            .expect("decode managed session"),
        )
    }

    async fn reconcilable_sessions(&self) -> Vec<ScopedPersistedSession> {
        let conn = self.conn.lock().expect("session store mutex poisoned");
        let mut statement = conn
            .prepare(
                "SELECT scope_id, session_id, aggregate_json, agent_id, model, title, metadata_json, environment_id, status, archived_at, effective_inputs_json, environment_binding, runtime_json, revision
                 FROM managed_session ORDER BY session_id",
            )
            .expect("prepare pending Session resource activations");
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    EncodedSessionRow {
                        session_id: row.get(1)?,
                        aggregate_json: row.get(2)?,
                        agent_id: row.get(3)?,
                        model: row.get(4)?,
                        title: row.get(5)?,
                        metadata_json: row.get(6)?,
                        environment_id: row.get(7)?,
                        status: row.get(8)?,
                        archived_at: row.get(9)?,
                        effective_inputs_json: row.get(10)?,
                        environment_binding: row.get(11)?,
                        runtime_json: row.get(12)?,
                        revision: row.get(13)?,
                    },
                ))
            })
            .expect("query pending Session resource activations")
            .map(|row| {
                let (workspace_id, row) = row.expect("read managed session");
                ScopedPersistedSession {
                    workspace_id,
                    session: decode(row).expect("decode managed session"),
                }
            })
            .filter(|record| {
                let session = &record.session;
                session.status == "deleted"
                    || session.resources.needs_reconciliation()
                    || (session.status != "idle" && session.resources.has_active())
                    || session.mcp.needs_reconciliation()
            })
            .collect()
    }

    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Option<awaken_session_contract::SessionIdempotencyReceipt> {
        let conn = self.conn.lock().expect("session store mutex poisoned");
        conn.query_row(
            "SELECT payload_hash, committed_revision FROM managed_session_idempotency
             WHERE session_id = ?1 AND idempotency_key = ?2",
            params![session_id, key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .expect("read Session idempotency receipt")
        .map(|(payload_hash, revision)| {
            awaken_session_contract::SessionIdempotencyReceipt {
                payload_hash,
                committed_revision: SessionRevision(
                    u64::try_from(revision).expect("nonnegative Session revision"),
                ),
            }
        })
    }

    async fn owner(&self, session_id: &str) -> Option<String> {
        let conn = self.conn.lock().expect("session store mutex poisoned");
        conn.query_row(
            "SELECT scope_id FROM managed_session WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .optional()
        .expect("read managed session owner")
    }
}

/// A Postgres-backed [`ManagedSessionRepository`] — the network-DB sibling over
/// the same `managed` migration scope. The port is async, so this is a plain sqlx
/// adapter (no sync bridge needed).
pub struct PostgresManagedSessionRepository {
    pool: PgPool,
}

impl PostgresManagedSessionRepository {
    /// Connect and apply the session migrations under the `managed` namespace.
    pub async fn connect(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool: apply the session migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, String> {
        let bundle = session_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self { pool })
    }

    /// Connect to a schema migrated by an operational command without DDL.
    pub async fn connect_existing(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        let bundle = session_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .verify_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl ManagedSessionRepository for PostgresManagedSessionRepository {
    async fn create(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionRevision, SessionRepositoryError> {
        if session.session_id.trim().is_empty()
            || idempotency.key.trim().is_empty()
            || idempotency.payload_hash.trim().is_empty()
            || session.revision != SessionRevision(0)
            || lifecycle_facts
                .iter()
                .any(|fact| fact.object_id != session.session_id)
        {
            return Err(SessionRepositoryError::InvalidMutation(
                "invalid Session create command".into(),
            ));
        }
        let mut tx = self.pool.begin().await.map_err(storage)?;
        if let Some(row) = sqlx::query(
            "SELECT payload_hash, committed_revision FROM managed_session_idempotency
             WHERE session_id = $1 AND idempotency_key = $2",
        )
        .bind(&session.session_id)
        .bind(&idempotency.key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        {
            let stored_hash: String = row.get("payload_hash");
            if stored_hash != idempotency.payload_hash {
                return Err(SessionRepositoryError::IdempotencyMismatch);
            }
            let committed_revision: i64 = row.get("committed_revision");
            return u64::try_from(committed_revision)
                .map(SessionRevision)
                .map_err(|_| storage("negative committed Session revision"));
        }
        if sqlx::query("SELECT 1 FROM managed_session_tombstone WHERE session_id = $1")
            .bind(&session.session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?
            .is_some()
        {
            return Err(SessionRepositoryError::Tombstoned);
        }
        let new_revision = SessionRevision(1);
        session.revision = new_revision;
        let inserted = sqlx::query(
            r#"INSERT INTO managed_session
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json,
                 scope_id, status, archived_at, effective_inputs_json, environment_binding,
                 runtime_json, revision, aggregate_json)
             VALUES ($1, '', '', NULL, '{}', '', '[]', $2, 'aggregate', NULL,
                     '{"inputs":[]}', NULL,
                     '{"mcp_servers":[],"runtime":null,"deny_egress":false,"sandbox":null}',
                     $3, $4)
             ON CONFLICT (session_id) DO NOTHING"#,
        )
        .bind(&session.session_id)
        .bind(owner_scope)
        .bind(db_revision(new_revision)?)
        .bind(aggregate_str(&session))
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        if inserted != 1 {
            return Err(SessionRepositoryError::AlreadyExists);
        }
        sqlx::query(
            "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&session.session_id)
        .bind(&idempotency.key)
        .bind(&idempotency.payload_hash)
        .bind(db_revision(new_revision)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        for fact in lifecycle_facts {
            sqlx::query(
                "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
                 ON CONFLICT (fact_id) DO NOTHING",
            )
            .bind(&fact.id)
            .bind(lifecycle_str(&fact))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)?;
        Ok(new_revision)
    }

    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        let next = mutation
            .validate()
            .map_err(|error| SessionRepositoryError::InvalidMutation(error.to_string()))?;
        let session_id = mutation.payload.session_id().to_string();
        let mut tx = self.pool.begin().await.map_err(storage)?;
        if let Some(row) = sqlx::query(
            "SELECT payload_hash, committed_revision FROM managed_session_idempotency
             WHERE session_id = $1 AND idempotency_key = $2",
        )
        .bind(&session_id)
        .bind(&mutation.idempotency.key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        {
            let stored_hash: String = row.get("payload_hash");
            if stored_hash != mutation.idempotency.payload_hash {
                return Ok(SessionMutationResult::IdempotencyMismatch);
            }
            let committed_revision: i64 = row.get("committed_revision");
            return Ok(SessionMutationResult::Replayed {
                new_revision: SessionRevision(
                    u64::try_from(committed_revision)
                        .map_err(|_| storage("negative committed Session revision"))?,
                ),
            });
        }
        let current = sqlx::query(
            "SELECT revision, scope_id FROM managed_session WHERE session_id = $1 FOR UPDATE",
        )
        .bind(&session_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(current) = current else {
            let tombstone = sqlx::query(
                "SELECT deleted_revision FROM managed_session_tombstone WHERE session_id = $1",
            )
            .bind(&session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
            let revision = tombstone.map_or(0, |row| row.get::<i64, _>("deleted_revision"));
            return Ok(SessionMutationResult::Conflict {
                current_revision: SessionRevision(u64::try_from(revision).unwrap_or_default()),
            });
        };
        let current_revision = SessionRevision(
            u64::try_from(current.get::<i64, _>("revision"))
                .map_err(|_| storage("negative managed Session revision"))?,
        );
        let current_owner: String = current.get("scope_id");
        if current_owner != owner_scope || current_revision != mutation.expected_revision {
            return Ok(SessionMutationResult::Conflict { current_revision });
        }
        match &mutation.payload {
            SessionMutationPayload::Replace(replacement) => {
                let mut replacement = replacement.clone();
                replacement.revision = next;
                let affected = sqlx::query(
                    "UPDATE managed_session SET
                        aggregate_json = $2, revision = $3
                     WHERE session_id = $1 AND scope_id = $4 AND revision = $5",
                )
                .bind(&replacement.session_id)
                .bind(aggregate_str(&replacement))
                .bind(db_revision(next)?)
                .bind(owner_scope)
                .bind(db_revision(current_revision)?)
                .execute(&mut *tx)
                .await
                .map_err(storage)?
                .rows_affected();
                if affected != 1 {
                    return Ok(SessionMutationResult::Conflict { current_revision });
                }
            }
            SessionMutationPayload::Delete(tombstone) => {
                let affected = sqlx::query(
                    "DELETE FROM managed_session
                     WHERE session_id = $1 AND scope_id = $2 AND revision = $3",
                )
                .bind(&session_id)
                .bind(owner_scope)
                .bind(db_revision(current_revision)?)
                .execute(&mut *tx)
                .await
                .map_err(storage)?
                .rows_affected();
                if affected != 1 {
                    return Ok(SessionMutationResult::Conflict { current_revision });
                }
                sqlx::query(
                    "INSERT INTO managed_session_tombstone
                        (session_id, scope_id, deleted_revision, deleted_at)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(&tombstone.session_id)
                .bind(owner_scope)
                .bind(db_revision(tombstone.deleted_revision)?)
                .bind(&tombstone.deleted_at)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
            }
        }
        sqlx::query(
            "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&session_id)
        .bind(&mutation.idempotency.key)
        .bind(&mutation.idempotency.payload_hash)
        .bind(db_revision(next)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        for fact in mutation.lifecycle_facts {
            sqlx::query(
                "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
                 ON CONFLICT (fact_id) DO NOTHING",
            )
            .bind(&fact.id)
            .bind(lifecycle_str(&fact))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)?;
        Ok(SessionMutationResult::Applied { new_revision: next })
    }

    async fn append_lifecycle(&self, fact: ManagedLifecycleFact) {
        sqlx::query(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
             ON CONFLICT (fact_id) DO NOTHING",
        )
        .bind(&fact.id)
        .bind(lifecycle_str(&fact))
        .execute(&self.pool)
        .await
        .expect("append session lifecycle fact");
    }

    async fn pending_lifecycle(&self) -> Vec<ManagedLifecycleFact> {
        sqlx::query("SELECT data FROM managed_lifecycle_outbox ORDER BY created_at, fact_id")
            .fetch_all(&self.pool)
            .await
            .expect("read pending lifecycle facts")
            .into_iter()
            .map(|row| {
                let data: String = row.get("data");
                decode_lifecycle(&data).expect("decode lifecycle fact")
            })
            .collect()
    }

    async fn complete_lifecycle(&self, fact_id: &str) {
        sqlx::query("DELETE FROM managed_lifecycle_outbox WHERE fact_id = $1")
            .bind(fact_id)
            .execute(&self.pool)
            .await
            .expect("complete lifecycle fact");
    }

    async fn get(&self, session_id: &str) -> Option<PersistedSession> {
        let row = sqlx::query(
            "SELECT aggregate_json, agent_id, model, title, metadata_json, environment_id, status, archived_at, effective_inputs_json, environment_binding, runtime_json, revision \
             FROM managed_session WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .expect("read managed session")?;
        let metadata_json: String = row.get("metadata_json");
        let effective_inputs_json: String = row.get("effective_inputs_json");
        Some(
            decode(EncodedSessionRow {
                aggregate_json: row.get("aggregate_json"),
                session_id: session_id.to_string(),
                agent_id: row.get("agent_id"),
                model: row.get("model"),
                title: row.get("title"),
                metadata_json,
                environment_id: row.get("environment_id"),
                effective_inputs_json,
                environment_binding: row.get("environment_binding"),
                runtime_json: row.get("runtime_json"),
                status: row.get("status"),
                archived_at: row.get("archived_at"),
                revision: row.get("revision"),
            })
            .expect("decode managed session"),
        )
    }

    async fn reconcilable_sessions(&self) -> Vec<ScopedPersistedSession> {
        sqlx::query(
            "SELECT scope_id, session_id, aggregate_json, agent_id, model, title, metadata_json, environment_id, status, archived_at, effective_inputs_json, environment_binding, runtime_json, revision \
             FROM managed_session ORDER BY session_id",
        )
        .fetch_all(&self.pool)
        .await
        .expect("read pending Session resource activations")
        .into_iter()
        .map(|row| ScopedPersistedSession {
            workspace_id: row.get("scope_id"),
            session: decode(EncodedSessionRow {
                aggregate_json: row.get("aggregate_json"),
                session_id: row.get("session_id"),
                agent_id: row.get("agent_id"),
                model: row.get("model"),
                title: row.get("title"),
                metadata_json: row.get("metadata_json"),
                environment_id: row.get("environment_id"),
                status: row.get("status"),
                archived_at: row.get("archived_at"),
                effective_inputs_json: row.get("effective_inputs_json"),
                environment_binding: row.get("environment_binding"),
                runtime_json: row.get("runtime_json"),
                revision: row.get("revision"),
            })
            .expect("decode managed session"),
        })
        .filter(|record| {
            let session = &record.session;
            session.status == "deleted"
                || session.resources.needs_reconciliation()
                || (session.status != "idle" && session.resources.has_active())
                || session.mcp.needs_reconciliation()
        })
        .collect()
    }

    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Option<awaken_session_contract::SessionIdempotencyReceipt> {
        let row = sqlx::query(
            "SELECT payload_hash, committed_revision FROM managed_session_idempotency
             WHERE session_id = $1 AND idempotency_key = $2",
        )
        .bind(session_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .expect("read Session idempotency receipt")?;
        let revision: i64 = row.get("committed_revision");
        Some(awaken_session_contract::SessionIdempotencyReceipt {
            payload_hash: row.get("payload_hash"),
            committed_revision: SessionRevision(
                u64::try_from(revision).expect("nonnegative Session revision"),
            ),
        })
    }

    async fn owner(&self, session_id: &str) -> Option<String> {
        let row = sqlx::query("SELECT scope_id FROM managed_session WHERE session_id = $1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await
            .expect("read managed session owner")?;
        Some(row.get("scope_id"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use awaken_credential_contract::{
        CredentialRealizationProfile, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_session_contract::{
        EnvironmentFingerprint, EnvironmentSnapshot, McpAttachmentDraft, McpAttachmentOrigin,
        McpTarget, SessionBaseline, SessionBaselineState, SessionMcpAttachmentSet,
        SessionMcpAuthoringContext, SessionNetworkPolicy,
    };

    use super::*;

    /// Shared-file write-admission causal graph:
    /// another aggregate owns the SQLite writer reservation × wait budget.
    ///
    /// | Rule | competing writer | wait budget | Result |
    /// |---|---|---|---|
    /// | W1 | no | any | commit immediately |
    /// | W2 | yes | sufficient | wait, then commit once |
    /// | W3 | yes | exhausted | storage failure |
    ///
    /// W1 is covered by every SQLite repository test; this case protects W2.
    /// SQLite itself owns W3 and returns the typed storage failure after the
    /// configured bound, so the repository does not add a parallel retry loop.
    #[test]
    fn sqlite_create_waits_for_a_competing_aggregate_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.db");
        let path = path.to_string_lossy().to_string();
        let repo = Arc::new(SqliteManagedSessionRepository::open(&path).unwrap());
        let mut blocker = Connection::open(&path).unwrap();
        let blocker_tx = blocker
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let writer = {
            let repo = repo.clone();
            std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(create_fixture(
                        repo.as_ref(),
                        "default",
                        sample("sesn_waiting_writer"),
                        Vec::new(),
                    ))
            })
        };
        started_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        blocker_tx.commit().unwrap();

        let created = writer.join().unwrap();
        assert_eq!(created.session_id, "sesn_waiting_writer", "W2");
    }

    fn sample(id: &str) -> PersistedSession {
        let mut metadata = BTreeMap::new();
        metadata.insert("team".to_string(), "research".to_string());
        PersistedSession {
            session_id: id.to_string(),
            revision: Default::default(),
            baseline: SessionBaselineState::Frozen(SessionBaseline::compile(
                awaken_session_contract::SessionBaselineInputs {
                    environment: EnvironmentSnapshot {
                        environment_id: "env_local".into(),
                        revision: awaken_session_contract::env_registry::EnvironmentRevision(1),
                        config_fingerprint: EnvironmentFingerprint("env-fingerprint".into()),
                        sandbox: serde_json::json!({}),
                        packages: Default::default(),
                        network: SessionNetworkPolicy::Unrestricted,
                        credential_realization: CredentialRealizationProfile {
                            inference_holder: PlaintextHolder::new(
                                PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                            mcp_holder: PlaintextHolder::new(
                                PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                            resource_holder: PlaintextHolder::new(
                                PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                        },
                    },
                    mcp_authoring: SessionMcpAuthoringContext::default(),
                    agent_id: "coder".into(),
                    toolsets: Vec::new(),
                    model: "kimi-k2".into(),
                    runtime: None,
                    application: None,
                    delegate_ids: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                },
            )),
            title: Some("My session".to_string()),
            metadata,
            agent_tools: Vec::new(),
            environment_binding: None,
            mcp: SessionMcpAttachmentSet::from_initial(
                vec![McpAttachmentDraft {
                    name: "calc".into(),
                    target: McpTarget::parse_http("https://x").unwrap(),
                    credential: None,
                    origin: McpAttachmentOrigin::Session,
                }],
                None,
            )
            .unwrap(),
            resources: awaken_session_contract::SessionResourceState::from_legacy(
                serde_json::from_value(serde_json::json!({
                    "inputs": [{
                        "binding_id": "input-file",
                        "source": { "kind": "file", "file_id": "file-1" },
                        "mount_path": "/mnt/input",
                        "access": "read_only"
                    }]
                }))
                .unwrap(),
            ),
            realization: None,
            status: "idle".into(),
            archived_at: None,
        }
    }

    fn fact(id: &str, session_id: &str, event_type: &str) -> ManagedLifecycleFact {
        ManagedLifecycleFact {
            id: id.into(),
            object_id: session_id.into(),
            workspace_id: Some("ws_a".into()),
            event_type: event_type.into(),
            timestamp: 1_700_000_000,
        }
    }

    async fn create_fixture<R: ManagedSessionRepository>(
        repo: &R,
        owner: &str,
        mut session: PersistedSession,
        facts: Vec<ManagedLifecycleFact>,
    ) -> PersistedSession {
        session.revision = SessionRevision(0);
        let payload = SessionMutationPayload::Replace(session.clone());
        let payload_hash = payload.stable_hash();
        session.revision = repo
            .create(
                owner,
                session.clone(),
                IdempotencyRecord {
                    key: format!("test:create:{}:{payload_hash}", session.session_id),
                    payload_hash,
                },
                facts,
            )
            .await
            .expect("create Session fixture");
        session
    }

    async fn replace_fixture<R: ManagedSessionRepository>(
        repo: &R,
        owner: &str,
        mut session: PersistedSession,
        key: &str,
        facts: Vec<ManagedLifecycleFact>,
    ) -> PersistedSession {
        session.revision = repo.get(&session.session_id).await.unwrap().revision;
        let payload = SessionMutationPayload::Replace(session.clone());
        let payload_hash = payload.stable_hash();
        let result = repo
            .commit_mutation(
                owner,
                SessionMutation {
                    expected_revision: session.revision,
                    idempotency: IdempotencyRecord {
                        key: key.into(),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: facts,
                },
            )
            .await
            .expect("replace Session fixture");
        session.revision = match result {
            SessionMutationResult::Applied { new_revision }
            | SessionMutationResult::Replayed { new_revision } => new_revision,
            other => panic!("replace Session fixture failed: {other:?}"),
        };
        session
    }

    async fn delete_fixture<R: ManagedSessionRepository>(
        repo: &R,
        owner: &str,
        session_id: &str,
        fact: ManagedLifecycleFact,
    ) {
        let current = repo.get(session_id).await.unwrap();
        let payload = SessionMutationPayload::Delete(awaken_session_contract::SessionTombstone {
            session_id: session_id.into(),
            deleted_revision: SessionRevision(current.revision.0 + 1),
            deleted_at: fact.timestamp.to_string(),
        });
        let payload_hash = payload.stable_hash();
        assert!(matches!(
            repo.commit_mutation(
                owner,
                SessionMutation {
                    expected_revision: current.revision,
                    idempotency: IdempotencyRecord {
                        key: format!("test:delete:{session_id}:{payload_hash}"),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: vec![fact],
                },
            )
            .await
            .unwrap(),
            SessionMutationResult::Applied { .. }
        ));
    }

    /// Root-mutation cause graph shared by every durable backend:
    /// C1=idempotency key exists, C2=payload hash matches, C3=root revision
    /// matches, C4=aggregate was tombstoned. Key/hash resolution precedes CAS,
    /// so response-loss replay remains deterministic after delete.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | effect |
    /// |---|---|---|---|---|---|
    /// | R1 | F | - | T | F | apply |
    /// | R2 | T | T | - | F/T | replay |
    /// | R3 | T | F | - | F/T | idempotency mismatch |
    /// | R4 | F | - | F | F | revision conflict |
    /// | R5 | F | - | - | T | tombstone conflict |
    async fn root_mutation_decision_table<R: ManagedSessionRepository>(repo: &R, id: &str) {
        let created = create_fixture(repo, "ws_a", sample(id), Vec::new()).await;
        let mut replacement = created.clone();
        replacement.title = Some("winner".into());
        let replace_payload = SessionMutationPayload::Replace(replacement);
        let replace_hash = replace_payload.stable_hash();
        let replace = || SessionMutation {
            expected_revision: created.revision,
            idempotency: IdempotencyRecord {
                key: format!("decision:{id}:replace"),
                payload_hash: replace_hash.clone(),
            },
            payload: replace_payload.clone(),
            lifecycle_facts: Vec::new(),
        };
        assert!(
            matches!(
                repo.commit_mutation("ws_a", replace()).await.unwrap(),
                SessionMutationResult::Applied {
                    new_revision: SessionRevision(2)
                }
            ),
            "R1"
        );
        assert!(
            matches!(
                repo.commit_mutation("ws_a", replace()).await.unwrap(),
                SessionMutationResult::Replayed {
                    new_revision: SessionRevision(2)
                }
            ),
            "R2"
        );

        let mut mismatched = replace();
        mismatched.idempotency.payload_hash = "another-hash".into();
        assert_eq!(
            repo.commit_mutation("ws_a", mismatched).await.unwrap(),
            SessionMutationResult::IdempotencyMismatch,
            "R3"
        );
        let mut stale = replace();
        stale.idempotency.key = format!("decision:{id}:stale");
        assert_eq!(
            repo.commit_mutation("ws_a", stale).await.unwrap(),
            SessionMutationResult::Conflict {
                current_revision: SessionRevision(2)
            },
            "R4"
        );

        let delete_payload =
            SessionMutationPayload::Delete(awaken_session_contract::SessionTombstone {
                session_id: id.into(),
                deleted_revision: SessionRevision(3),
                deleted_at: "3".into(),
            });
        let delete_hash = delete_payload.stable_hash();
        let delete = || SessionMutation {
            expected_revision: SessionRevision(2),
            idempotency: IdempotencyRecord {
                key: format!("decision:{id}:delete"),
                payload_hash: delete_hash.clone(),
            },
            payload: delete_payload.clone(),
            lifecycle_facts: Vec::new(),
        };
        assert!(
            matches!(
                repo.commit_mutation("ws_a", delete()).await.unwrap(),
                SessionMutationResult::Applied {
                    new_revision: SessionRevision(3)
                }
            ),
            "R1 delete"
        );
        assert!(
            matches!(
                repo.commit_mutation("ws_a", delete()).await.unwrap(),
                SessionMutationResult::Replayed {
                    new_revision: SessionRevision(3)
                }
            ),
            "R2 tombstone replay"
        );
        let mut after_delete = replace();
        after_delete.expected_revision = SessionRevision(3);
        after_delete.idempotency.key = format!("decision:{id}:after-delete");
        if let SessionMutationPayload::Replace(session) = &mut after_delete.payload {
            session.revision = SessionRevision(3);
        }
        after_delete.idempotency.payload_hash = after_delete.payload.stable_hash();
        assert_eq!(
            repo.commit_mutation("ws_a", after_delete).await.unwrap(),
            SessionMutationResult::Conflict {
                current_revision: SessionRevision(3)
            },
            "R5"
        );
    }

    fn extraction(id: &str, key: &str) -> awaken_ext_memory::MemoryExtractionIntent {
        awaken_ext_memory::MemoryExtractionIntent::new_range(
            id,
            key,
            "ws-a",
            "sesn-1",
            "terminal-1",
            "memory-1",
            1,
            1,
            1,
            Vec::new(),
            awaken_ext_memory::MemoryExtractorSnapshot {
                instructions: Some("extract durable facts".into()),
                ..awaken_ext_memory::MemoryExtractorSnapshot::host_executor(
                    "memory-agent",
                    "host",
                    "model-1",
                    "host",
                )
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn extraction_intent_and_claim_survive_sqlite_reopen() {
        use awaken_ext_memory::{
            MemoryExtractionRepository, MemoryExtractionStatus, PutMemoryExtractionOutcome,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let path = path.to_string_lossy().to_string();
        {
            let repo = SqliteManagedSessionRepository::open(&path).unwrap();
            let initial = extraction("extract-1", "terminal-1");
            assert_eq!(
                repo.put_extraction_if_absent(initial.clone())
                    .await
                    .unwrap(),
                PutMemoryExtractionOutcome::Inserted
            );
            assert_eq!(
                repo.put_extraction_if_absent(initial).await.unwrap(),
                PutMemoryExtractionOutcome::Existing
            );
        }

        let repo = SqliteManagedSessionRepository::open(&path).unwrap();
        let mut claimed = repo.get_extraction("extract-1").await.unwrap().unwrap();
        let expected_revision = claimed.revision;
        claimed.claim("worker-a", 100, 50).unwrap();
        repo.compare_and_swap_extraction(expected_revision, claimed.clone())
            .await
            .unwrap();
        drop(repo);

        let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
        let recovered = reopened.recoverable_extractions(10).await.unwrap();
        assert_eq!(reopened.extraction_cursor("sesn-1").await.unwrap(), 1);
        assert_eq!(recovered, vec![claimed]);
        assert_eq!(recovered[0].status, MemoryExtractionStatus::Claimed);
        assert!(matches!(
            reopened
                .compare_and_swap_extraction(0, recovered[0].clone())
                .await,
            Err(awaken_ext_memory::MemoryExtractionError::RevisionConflict(
                _
            ))
        ));
    }

    #[tokio::test]
    async fn lifecycle_fact_survives_the_commit_to_notification_crash_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions-outbox.db");
        let path = path.to_string_lossy().to_string();
        {
            let repo = SqliteManagedSessionRepository::open(&path).unwrap();
            create_fixture(
                &repo,
                "ws_a",
                sample("sesn_tx"),
                vec![fact(
                    "session:sesn_tx:created",
                    "sesn_tx",
                    "session.status_idled",
                )],
            )
            .await;
            // Simulated hard crash: the lifecycle sink is deliberately never called.
        }

        let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
        assert_eq!(reopened.owner("sesn_tx").await.as_deref(), Some("ws_a"));
        let pending = reopened.pending_lifecycle().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "session:sesn_tx:created");

        reopened.complete_lifecycle(&pending[0].id).await;
        reopened.complete_lifecycle(&pending[0].id).await;
        assert!(reopened.pending_lifecycle().await.is_empty());
    }

    #[tokio::test]
    async fn environment_binding_survives_sqlite_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions-binding.db");
        let path = path.to_string_lossy().to_string();
        {
            let repo = SqliteManagedSessionRepository::open(&path).unwrap();
            create_fixture(&repo, "ws_a", sample("sesn_bound"), Vec::new()).await;
            let mut bound = repo.get("sesn_bound").await.unwrap();
            bound.environment_binding = Some("opaque-binding".into());
            replace_fixture(&repo, "ws_a", bound, "test:bind", Vec::new()).await;
        }
        let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
        assert_eq!(
            reopened
                .get("sesn_bound")
                .await
                .and_then(|session| session.environment_binding),
            Some("opaque-binding".to_string())
        );
        assert_eq!(reopened.owner("sesn_bound").await.as_deref(), Some("ws_a"));
    }

    #[tokio::test]
    async fn terminal_state_and_its_fact_share_one_repository_commit() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        create_fixture(
            &repo,
            "ws_a",
            sample("sesn_terminal"),
            vec![fact("created", "sesn_terminal", "session.status_idled")],
        )
        .await;
        repo.complete_lifecycle("created").await;

        let mut terminal = repo.get("sesn_terminal").await.unwrap();
        terminal.status = "terminated".into();
        terminal.archived_at = Some("2026-01-01T00:00:00Z".into());
        replace_fixture(
            &repo,
            "ws_a",
            terminal,
            "test:terminal",
            vec![fact(
                "terminated",
                "sesn_terminal",
                "session.status_terminated",
            )],
        )
        .await;
        let archived = repo.get("sesn_terminal").await.unwrap();
        assert_eq!(archived.status, "terminated");
        assert_eq!(
            archived.archived_at.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(repo.pending_lifecycle().await[0].id, "terminated");

        repo.complete_lifecycle("terminated").await;
        delete_fixture(
            &repo,
            "ws_a",
            "sesn_terminal",
            fact("deleted", "sesn_terminal", "session.deleted"),
        )
        .await;
        assert!(repo.get("sesn_terminal").await.is_none());
        assert_eq!(repo.pending_lifecycle().await[0].id, "deleted");
    }

    #[tokio::test]
    async fn sqlite_root_mutation_decision_table_is_atomic() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        root_mutation_decision_table(&repo, "sesn_sqlite_decisions").await;
    }

    #[tokio::test]
    async fn round_trips_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let path = path.to_string_lossy().to_string();

        // First process: create + persist, then drop the repo (simulated exit).
        {
            let repo = SqliteManagedSessionRepository::open(&path).unwrap();
            let expected = create_fixture(&repo, "default", sample("sesn_1"), Vec::new()).await;
            assert_eq!(repo.get("sesn_1").await, Some(expected));
        }
        // Second process: a fresh repo over the same file restores the row.
        let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
        let mut expected = sample("sesn_1");
        expected.revision = SessionRevision(1);
        assert_eq!(reopened.get("sesn_1").await, Some(expected));
        assert!(reopened.get("sesn_missing").await.is_none());
    }

    #[tokio::test]
    async fn legacy_manifest_rows_upgrade_to_resource_state_on_read() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        let legacy = serde_json::json!({
            "inputs": [{
                "binding_id": "legacy-file",
                "source": { "kind": "file", "file_id": "file-old" },
                "mount_path": "/legacy",
                "access": "read_only"
            }]
        })
        .to_string();
        repo.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO managed_session
                 (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json, effective_inputs_json)
                 VALUES (?1, 'agent', 'model', NULL, '{}', 'env', '[]', ?2)",
                params!["legacy", legacy],
            )
            .unwrap();

        let loaded = repo.get("legacy").await.unwrap();
        assert_eq!(loaded.resources.revision, 1);
        assert_eq!(loaded.resources.active.inputs.len(), 1);
        assert!(loaded.resources.activations.is_empty());
        assert!(!loaded.resources.needs_reconciliation());
    }

    #[tokio::test]
    async fn save_is_an_idempotent_upsert() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        create_fixture(&repo, "default", sample("sesn_1"), Vec::new()).await;
        let mut updated = sample("sesn_1");
        updated.title = Some("Renamed".to_string());
        updated = replace_fixture(&repo, "default", updated, "test:rename", Vec::new()).await;
        assert_eq!(
            repo.get("sesn_1").await,
            Some(updated),
            "re-save overwrites"
        );
    }

    #[tokio::test]
    async fn owner_scope_is_recorded_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let path = path.to_string_lossy().to_string();
        {
            let repo = SqliteManagedSessionRepository::open(&path).unwrap();
            create_fixture(&repo, "ws_a", sample("sesn_1"), Vec::new()).await;
            assert_eq!(repo.owner("sesn_1").await, Some("ws_a".to_string()));
        }
        // After a restart the owner is still readable — the cross-process fence input
        // for the edge ownership guard (ADR-0051).
        let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
        assert_eq!(reopened.owner("sesn_1").await, Some("ws_a".to_string()));
        // A row saved but never owner-stamped defaults to the seeded scope.
        create_fixture(&reopened, "default", sample("sesn_2"), Vec::new()).await;
        assert_eq!(reopened.owner("sesn_2").await, Some("default".to_string()));
        // An unknown session has no owner.
        assert_eq!(reopened.owner("sesn_missing").await, None);
    }

    /// Persistence authority causal graph:
    /// aggregate present -> decode aggregate only; aggregate absent -> migrate
    /// legacy columns once. Corruption in the selected authority fails loudly.
    ///
    /// | Rule | aggregate present | selected payload valid | legacy valid | Effect |
    /// |---|---|---|---|---|
    /// | P1 | T | T | - | aggregate |
    /// | P2 | T | F | - | fail closed |
    /// | P3 | F | T | T | legacy migration |
    /// | P4 | F | F | F | fail closed |
    #[tokio::test]
    #[should_panic(expected = "decode managed session")]
    async fn corrupt_canonical_aggregate_fails_closed() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        create_fixture(&repo, "default", sample("sesn_1"), Vec::new()).await;
        {
            let conn = repo.conn.lock().unwrap();
            conn.execute(
                "UPDATE managed_session \
                 SET aggregate_json = ?2 WHERE session_id = ?1",
                params!["sesn_1", "{not valid json"],
            )
            .unwrap();
        }
        let _ = repo.get("sesn_1").await;
    }

    #[tokio::test]
    async fn legacy_columns_are_not_a_parallel_authority() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        let mut expected = sample("sesn_1");
        expected = create_fixture(&repo, "default", expected, Vec::new()).await;
        repo.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE managed_session SET metadata_json = ?2, mcp_json = ?3,
                 runtime_json = ?4 WHERE session_id = ?1",
                params!["sesn_1", "{bad", "{bad", "{bad"],
            )
            .unwrap();
        assert_eq!(repo.get("sesn_1").await, Some(expected), "P1");
    }

    #[tokio::test]
    #[should_panic(expected = "decode managed session")]
    async fn corrupt_selected_legacy_payload_fails_closed() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        repo.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO managed_session
                 (session_id, agent_id, model, metadata_json, environment_id, mcp_json,
                  effective_inputs_json, runtime_json)
                 VALUES (?1, 'agent', 'model', ?2, 'env', '[]', '{\"inputs\":[]}',
                  '{\"mcp_servers\":[],\"runtime\":null,\"deny_egress\":false,\"sandbox\":null}')",
                params!["legacy-corrupt", "{bad"],
            )
            .unwrap();
        let _ = repo.get("legacy-corrupt").await;
    }

    /// Live Postgres round-trip, isolated in its own schema. Skips when no Postgres
    /// is reachable (`AWAKEN_TEST_DATABASE_URL`), proving the shared portable bundle
    /// and the same behavior on the network backend.
    #[tokio::test]
    async fn postgres_round_trips_and_upserts() {
        use awaken_ext_memory::{MemoryExtractionRepository, PutMemoryExtractionOutcome};
        use sqlx::Executor;
        use sqlx::postgres::{PgPool, PgPoolOptions};

        let url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        });
        let Ok(admin) = PgPool::connect(&url).await else {
            println!("[skip] no Postgres reachable");
            return;
        };
        let _ = admin
            .execute("DROP SCHEMA IF EXISTS t_managed_session CASCADE")
            .await;
        admin
            .execute("CREATE SCHEMA t_managed_session")
            .await
            .expect("create schema");
        admin.close().await;
        let pool = PgPoolOptions::new()
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    conn.execute("SET search_path = t_managed_session").await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("schema pool");
        let repo = PostgresManagedSessionRepository::with_pool(pool)
            .await
            .expect("store");

        root_mutation_decision_table(&repo, "sesn_pg_decisions").await;

        let created = create_fixture(&repo, "default", sample("sesn_1"), Vec::new()).await;
        assert_eq!(repo.get("sesn_1").await, Some(created));
        assert!(repo.get("sesn_missing").await.is_none());

        let mut updated = sample("sesn_1");
        updated.title = None; // exercises the nullable title column
        updated = replace_fixture(&repo, "default", updated, "test:pg-update", Vec::new()).await;
        assert_eq!(repo.get("sesn_1").await, Some(updated));

        create_fixture(
            &repo,
            "ws_a",
            sample("sesn_pg_tx"),
            vec![fact(
                "session:sesn_pg_tx:created",
                "sesn_pg_tx",
                "session.status_idled",
            )],
        )
        .await;
        assert_eq!(repo.owner("sesn_pg_tx").await.as_deref(), Some("ws_a"));
        assert_eq!(
            repo.pending_lifecycle().await[0].id,
            "session:sesn_pg_tx:created"
        );
        repo.complete_lifecycle("session:sesn_pg_tx:created").await;
        let mut terminal = repo.get("sesn_pg_tx").await.unwrap();
        terminal.status = "terminated".into();
        terminal.archived_at = Some("2026-07-19T00:00:00Z".into());
        replace_fixture(
            &repo,
            "ws_a",
            terminal,
            "test:pg-terminal",
            vec![fact(
                "session:sesn_pg_tx:terminated",
                "sesn_pg_tx",
                "session.status_terminated",
            )],
        )
        .await;
        assert_eq!(repo.get("sesn_pg_tx").await.unwrap().status, "terminated");
        assert_eq!(
            repo.pending_lifecycle().await[0].id,
            "session:sesn_pg_tx:terminated"
        );

        let initial = extraction("extract-pg", "terminal-pg");
        assert_eq!(
            repo.put_extraction_if_absent(initial.clone())
                .await
                .unwrap(),
            PutMemoryExtractionOutcome::Inserted
        );
        let mut claimed = initial;
        claimed.claim("worker-pg", 100, 50).unwrap();
        repo.compare_and_swap_extraction(0, claimed.clone())
            .await
            .unwrap();
        assert_eq!(
            repo.recoverable_extractions(10).await.unwrap(),
            vec![claimed]
        );
        assert_eq!(repo.extraction_cursor("sesn-1").await.unwrap(), 1);
    }

    /// Postgres parity for the ADR-0051 owner `scope_id` — the same atomic
    /// aggregate `create` / `commit_mutation` ownership assertions the SQLite test
    /// test has, which the pg test previously OMITTED. A second pool over the same schema
    /// stands in for a restart (the cross-process fence input the edge guard reads). Skips
    /// when no Postgres is reachable (`AWAKEN_TEST_DATABASE_URL`), isolated in its schema.
    #[tokio::test]
    async fn postgres_owner_scope_is_recorded_and_survives_a_reopen() {
        use sqlx::Executor;
        use sqlx::postgres::{PgPool, PgPoolOptions};

        let url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        });
        let Ok(admin) = PgPool::connect(&url).await else {
            println!("[skip] no Postgres reachable");
            return;
        };
        let _ = admin
            .execute("DROP SCHEMA IF EXISTS t_managed_session_owner CASCADE")
            .await;
        admin
            .execute("CREATE SCHEMA t_managed_session_owner")
            .await
            .expect("create schema");
        admin.close().await;

        let pool = || {
            let url = url.clone();
            async move {
                PgPoolOptions::new()
                    .after_connect(|conn, _meta| {
                        Box::pin(async move {
                            conn.execute("SET search_path = t_managed_session_owner")
                                .await?;
                            Ok(())
                        })
                    })
                    .connect(&url)
                    .await
                    .expect("schema pool")
            }
        };

        // First "process": save the row and owner atomically.
        let repo = PostgresManagedSessionRepository::with_pool(pool().await)
            .await
            .expect("store");
        create_fixture(&repo, "ws_a", sample("sesn_1"), Vec::new()).await;
        assert_eq!(repo.owner("sesn_1").await, Some("ws_a".to_string()));

        // Second "process": a fresh pool over the same schema still reads the owner.
        let reopened = PostgresManagedSessionRepository::with_pool(pool().await)
            .await
            .expect("store");
        assert_eq!(reopened.owner("sesn_1").await, Some("ws_a".to_string()));
        // A row saved but never owner-stamped defaults to the seeded scope.
        create_fixture(&reopened, "default", sample("sesn_2"), Vec::new()).await;
        assert_eq!(reopened.owner("sesn_2").await, Some("default".to_string()));
        // An unknown session has no owner.
        assert_eq!(reopened.owner("sesn_missing").await, None);
    }
}
