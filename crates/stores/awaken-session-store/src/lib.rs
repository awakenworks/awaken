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

use async_trait::async_trait;
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use awaken_session_contract::{
    ManagedSessionRepository, PersistedSession, ResolvedSessionResources, SessionLifecycleFact,
    SessionResourceState,
};

// The in-memory reference backends (plain + scoped) live here beside the durable
// siblings (issue A / Phase 1); the ports + PersistedSession value + the
// ScopedSessionRepo decorator stay inward in `awaken-session-contract`.
mod extraction;
mod inmem;
pub use inmem::{InMemoryScopedSessionStore, InMemorySessionRepository};
use rusqlite::{Connection, OptionalExtension, params};
use sqlx::Row;
use sqlx::postgres::PgPool;

/// The session store's table namespace / bundle prefix: the table is
/// `managed_session`, the ledger `managed_schema_migrations`.
const NS: &str = "managed";

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
        ],
    )
}

fn metadata_str(session: &PersistedSession) -> String {
    serde_json::to_string(&session.metadata).expect("session metadata serializes")
}

fn mcp_str(session: &PersistedSession) -> String {
    serde_json::to_string(&session.mcp_servers).expect("session mcp servers serialize")
}

fn effective_inputs_str(session: &PersistedSession) -> String {
    serde_json::to_string(&session.resources).expect("Session resource state serializes")
}

fn lifecycle_str(fact: &SessionLifecycleFact) -> String {
    serde_json::json!({
        "id": fact.id,
        "session_id": fact.session_id,
        "workspace_id": fact.workspace_id,
        "event_type": fact.event_type,
        "timestamp": fact.timestamp,
    })
    .to_string()
}

fn decode_lifecycle(data: &str) -> Result<SessionLifecycleFact, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(data)?;
    Ok(SessionLifecycleFact {
        id: value["id"].as_str().unwrap_or_default().to_string(),
        session_id: value["session_id"].as_str().unwrap_or_default().to_string(),
        workspace_id: value["workspace_id"].as_str().map(str::to_string),
        event_type: value["event_type"].as_str().unwrap_or_default().to_string(),
        timestamp: value["timestamp"].as_i64().unwrap_or_default(),
    })
}

/// Decode a persisted row's JSON payload columns. A corrupt column (truncated
/// write, manual edit, schema drift) surfaces as `Err` rather than silently
/// folding to an empty `metadata`/`mcp_servers` — that fail-open masked data loss
/// on read. Callers within this module treat it like any other unreadable row
/// (the module's `.expect` convention for read failures), so a corrupt row fails
/// loudly instead of returning a hollow session.
struct EncodedSessionRow {
    session_id: String,
    agent_id: String,
    model: String,
    title: Option<String>,
    metadata_json: String,
    environment_id: String,
    mcp_json: String,
    effective_inputs_json: String,
    status: String,
    archived_at: Option<String>,
}

fn decode_resource_state(data: &str) -> Result<SessionResourceState, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(data)?;
    if value.get("inputs").is_some() {
        return serde_json::from_value::<ResolvedSessionResources>(value)
            .map(SessionResourceState::from_legacy);
    }
    serde_json::from_value(value)
}

fn decode(row: EncodedSessionRow) -> Result<PersistedSession, serde_json::Error> {
    Ok(PersistedSession {
        session_id: row.session_id,
        agent_id: row.agent_id,
        model: row.model,
        title: row.title,
        metadata: serde_json::from_str(&row.metadata_json)?,
        environment_id: row.environment_id,
        mcp_servers: serde_json::from_str(&row.mcp_json)?,
        resources: decode_resource_state(&row.effective_inputs_json)?,
        status: row.status,
        archived_at: row.archived_at,
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
    async fn save_owned(&self, owner_scope: &str, session: PersistedSession) {
        let metadata_json = metadata_str(&session);
        let mcp_json = mcp_str(&session);
        let effective_inputs_json = effective_inputs_str(&session);
        let conn = self.conn.lock().expect("session store mutex poisoned");
        conn.execute(
            "INSERT INTO managed_session
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json, scope_id, status, archived_at, effective_inputs_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(session_id) DO UPDATE SET
                agent_id = excluded.agent_id,
                model = excluded.model,
                title = excluded.title,
                metadata_json = excluded.metadata_json,
                environment_id = excluded.environment_id,
                mcp_json = excluded.mcp_json,
                scope_id = excluded.scope_id,
                status = excluded.status,
                archived_at = excluded.archived_at,
                effective_inputs_json = excluded.effective_inputs_json",
            params![
                session.session_id,
                session.agent_id,
                session.model,
                session.title,
                metadata_json,
                session.environment_id,
                mcp_json,
                owner_scope,
                session.status,
                session.archived_at,
                effective_inputs_json,
            ],
        )
        .expect("persist managed session");
    }

    async fn save_owned_with_lifecycle(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        fact: SessionLifecycleFact,
    ) {
        let metadata_json = metadata_str(&session);
        let mcp_json = mcp_str(&session);
        let effective_inputs_json = effective_inputs_str(&session);
        let fact_id = fact.id.clone();
        let fact_json = lifecycle_str(&fact);
        let mut conn = self.conn.lock().expect("session store mutex poisoned");
        let tx = conn
            .transaction()
            .expect("begin session lifecycle transaction");
        tx.execute(
            "INSERT INTO managed_session
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json, scope_id, status, archived_at, effective_inputs_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(session_id) DO UPDATE SET
                agent_id = excluded.agent_id, model = excluded.model, title = excluded.title,
                metadata_json = excluded.metadata_json, environment_id = excluded.environment_id,
                mcp_json = excluded.mcp_json, scope_id = excluded.scope_id,
                status = excluded.status, archived_at = excluded.archived_at,
                effective_inputs_json = excluded.effective_inputs_json",
            params![
                session.session_id,
                session.agent_id,
                session.model,
                session.title,
                metadata_json,
                session.environment_id,
                mcp_json,
                owner_scope,
                session.status,
                session.archived_at,
                effective_inputs_json,
            ],
        )
        .expect("persist managed session in lifecycle transaction");
        tx.execute(
            "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
            params![fact_id, fact_json],
        )
        .expect("persist lifecycle fact in session transaction");
        tx.commit().expect("commit session lifecycle transaction");
    }

    async fn append_lifecycle(&self, fact: SessionLifecycleFact) {
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

    async fn archive_with_lifecycle(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: SessionLifecycleFact,
    ) {
        let data = lifecycle_str(&fact);
        let mut conn = self.conn.lock().expect("session store mutex poisoned");
        let tx = conn
            .transaction()
            .expect("begin archive lifecycle transaction");
        tx.execute(
            "UPDATE managed_session SET status = 'terminated', archived_at = ?2 WHERE session_id = ?1",
            params![session_id, archived_at],
        )
        .expect("archive managed session");
        tx.execute(
            "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
            params![fact.id, data],
        )
        .expect("persist archive lifecycle fact");
        tx.commit().expect("commit archive lifecycle transaction");
    }

    async fn delete_with_lifecycle(&self, session_id: &str, fact: SessionLifecycleFact) {
        let data = lifecycle_str(&fact);
        let mut conn = self.conn.lock().expect("session store mutex poisoned");
        let tx = conn
            .transaction()
            .expect("begin delete lifecycle transaction");
        tx.execute(
            "UPDATE managed_session SET status = 'deleted' WHERE session_id = ?1",
            params![session_id],
        )
        .expect("delete managed session");
        tx.execute(
            "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
            params![fact.id, data],
        )
        .expect("persist delete lifecycle fact");
        tx.commit().expect("commit delete lifecycle transaction");
    }

    async fn pending_lifecycle(&self) -> Vec<SessionLifecycleFact> {
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
                "SELECT agent_id, model, title, metadata_json, environment_id, mcp_json, status, archived_at, effective_inputs_json
                 FROM managed_session WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .optional()
            .expect("read managed session")?;
        let (
            agent_id,
            model,
            title,
            metadata_json,
            environment_id,
            mcp_json,
            status,
            archived_at,
            effective_inputs_json,
        ) = raw;
        Some(
            decode(EncodedSessionRow {
                session_id: session_id.to_string(),
                agent_id,
                model,
                title,
                metadata_json,
                environment_id,
                mcp_json,
                effective_inputs_json,
                status,
                archived_at,
            })
            .expect("decode managed session"),
        )
    }

    async fn pending_resource_sessions(&self) -> Vec<PersistedSession> {
        let conn = self.conn.lock().expect("session store mutex poisoned");
        let mut statement = conn
            .prepare(
                "SELECT session_id, agent_id, model, title, metadata_json, environment_id, mcp_json, status, archived_at, effective_inputs_json
                 FROM managed_session ORDER BY session_id",
            )
            .expect("prepare pending Session resource activations");
        statement
            .query_map([], |row| {
                Ok(EncodedSessionRow {
                    session_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    model: row.get(2)?,
                    title: row.get(3)?,
                    metadata_json: row.get(4)?,
                    environment_id: row.get(5)?,
                    mcp_json: row.get(6)?,
                    status: row.get(7)?,
                    archived_at: row.get(8)?,
                    effective_inputs_json: row.get(9)?,
                })
            })
            .expect("query pending Session resource activations")
            .map(|row| decode(row.expect("read managed session")).expect("decode managed session"))
            .filter(|session| {
                session.resources.needs_reconciliation()
                    || (session.status != "idle" && session.resources.has_active())
            })
            .collect()
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
}

#[async_trait]
impl ManagedSessionRepository for PostgresManagedSessionRepository {
    async fn save_owned(&self, owner_scope: &str, session: PersistedSession) {
        sqlx::query(
            "INSERT INTO managed_session \
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json, scope_id, status, archived_at, effective_inputs_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             ON CONFLICT (session_id) DO UPDATE SET \
                agent_id = excluded.agent_id, \
                model = excluded.model, \
                title = excluded.title, \
                metadata_json = excluded.metadata_json, \
                environment_id = excluded.environment_id, \
                mcp_json = excluded.mcp_json, \
                scope_id = excluded.scope_id, status = excluded.status, archived_at = excluded.archived_at, \
                effective_inputs_json = excluded.effective_inputs_json",
        )
        .bind(&session.session_id)
        .bind(&session.agent_id)
        .bind(&session.model)
        .bind(&session.title)
        .bind(metadata_str(&session))
        .bind(&session.environment_id)
        .bind(mcp_str(&session))
        .bind(owner_scope)
        .bind(&session.status)
        .bind(&session.archived_at)
        .bind(effective_inputs_str(&session))
        .execute(&self.pool)
        .await
        .expect("persist managed session");
    }

    async fn save_owned_with_lifecycle(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        fact: SessionLifecycleFact,
    ) {
        let mut tx = self
            .pool
            .begin()
            .await
            .expect("begin session lifecycle transaction");
        sqlx::query(
            "INSERT INTO managed_session
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json, scope_id, status, archived_at, effective_inputs_json)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (session_id) DO UPDATE SET
                agent_id = excluded.agent_id, model = excluded.model, title = excluded.title,
                metadata_json = excluded.metadata_json, environment_id = excluded.environment_id,
                mcp_json = excluded.mcp_json, scope_id = excluded.scope_id,
                status = excluded.status, archived_at = excluded.archived_at,
                effective_inputs_json = excluded.effective_inputs_json",
        )
        .bind(&session.session_id)
        .bind(&session.agent_id)
        .bind(&session.model)
        .bind(&session.title)
        .bind(metadata_str(&session))
        .bind(&session.environment_id)
        .bind(mcp_str(&session))
        .bind(owner_scope)
        .bind(&session.status)
        .bind(&session.archived_at)
        .bind(effective_inputs_str(&session))
        .execute(&mut *tx)
        .await
        .expect("persist managed session in lifecycle transaction");
        sqlx::query(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
             ON CONFLICT (fact_id) DO NOTHING",
        )
        .bind(&fact.id)
        .bind(lifecycle_str(&fact))
        .execute(&mut *tx)
        .await
        .expect("persist lifecycle fact in session transaction");
        tx.commit()
            .await
            .expect("commit session lifecycle transaction");
    }

    async fn append_lifecycle(&self, fact: SessionLifecycleFact) {
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

    async fn archive_with_lifecycle(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: SessionLifecycleFact,
    ) {
        let mut tx = self
            .pool
            .begin()
            .await
            .expect("begin archive lifecycle transaction");
        sqlx::query(
            "UPDATE managed_session SET status = 'terminated', archived_at = $2 WHERE session_id = $1",
        )
        .bind(session_id)
        .bind(archived_at)
        .execute(&mut *tx)
        .await
        .expect("archive managed session");
        sqlx::query(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
             ON CONFLICT (fact_id) DO NOTHING",
        )
        .bind(&fact.id)
        .bind(lifecycle_str(&fact))
        .execute(&mut *tx)
        .await
        .expect("persist archive lifecycle fact");
        tx.commit()
            .await
            .expect("commit archive lifecycle transaction");
    }

    async fn delete_with_lifecycle(&self, session_id: &str, fact: SessionLifecycleFact) {
        let mut tx = self
            .pool
            .begin()
            .await
            .expect("begin delete lifecycle transaction");
        sqlx::query("UPDATE managed_session SET status = 'deleted' WHERE session_id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .expect("delete managed session");
        sqlx::query(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
             ON CONFLICT (fact_id) DO NOTHING",
        )
        .bind(&fact.id)
        .bind(lifecycle_str(&fact))
        .execute(&mut *tx)
        .await
        .expect("persist delete lifecycle fact");
        tx.commit()
            .await
            .expect("commit delete lifecycle transaction");
    }

    async fn pending_lifecycle(&self) -> Vec<SessionLifecycleFact> {
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
            "SELECT agent_id, model, title, metadata_json, environment_id, mcp_json, status, archived_at, effective_inputs_json \
             FROM managed_session WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .expect("read managed session")?;
        let metadata_json: String = row.get("metadata_json");
        let mcp_json: String = row.get("mcp_json");
        let effective_inputs_json: String = row.get("effective_inputs_json");
        Some(
            decode(EncodedSessionRow {
                session_id: session_id.to_string(),
                agent_id: row.get("agent_id"),
                model: row.get("model"),
                title: row.get("title"),
                metadata_json,
                environment_id: row.get("environment_id"),
                mcp_json,
                effective_inputs_json,
                status: row.get("status"),
                archived_at: row.get("archived_at"),
            })
            .expect("decode managed session"),
        )
    }

    async fn pending_resource_sessions(&self) -> Vec<PersistedSession> {
        sqlx::query(
            "SELECT session_id, agent_id, model, title, metadata_json, environment_id, mcp_json, status, archived_at, effective_inputs_json \
             FROM managed_session ORDER BY session_id",
        )
        .fetch_all(&self.pool)
        .await
        .expect("read pending Session resource activations")
        .into_iter()
        .map(|row| {
            decode(EncodedSessionRow {
                session_id: row.get("session_id"),
                agent_id: row.get("agent_id"),
                model: row.get("model"),
                title: row.get("title"),
                metadata_json: row.get("metadata_json"),
                environment_id: row.get("environment_id"),
                mcp_json: row.get("mcp_json"),
                status: row.get("status"),
                archived_at: row.get("archived_at"),
                effective_inputs_json: row.get("effective_inputs_json"),
            })
            .expect("decode managed session")
        })
        .filter(|session| session.resources.needs_reconciliation() || (session.status != "idle" && session.resources.has_active()))
        .collect()
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

    use super::*;

    fn sample(id: &str) -> PersistedSession {
        let mut metadata = BTreeMap::new();
        metadata.insert("team".to_string(), "research".to_string());
        PersistedSession {
            session_id: id.to_string(),
            agent_id: "coder".to_string(),
            model: "kimi-k2".to_string(),
            title: Some("My session".to_string()),
            metadata,
            environment_id: "env_local".to_string(),
            mcp_servers: vec![serde_json::json!({"name":"calc","type":"url","url":"https://x"})],
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
            status: "idle".into(),
            archived_at: None,
        }
    }

    fn fact(id: &str, session_id: &str, event_type: &str) -> SessionLifecycleFact {
        SessionLifecycleFact {
            id: id.into(),
            session_id: session_id.into(),
            workspace_id: Some("ws_a".into()),
            event_type: event_type.into(),
            timestamp: 1_700_000_000,
        }
    }

    fn extraction(id: &str, key: &str) -> awaken_session_contract::MemoryExtractionIntent {
        awaken_session_contract::MemoryExtractionIntent::new(
            id,
            key,
            "ws-a",
            "sesn-1",
            "terminal-1",
            "memory-1",
            1,
            Vec::new(),
            awaken_session_contract::MemoryExtractorSnapshot {
                agent_id: "memory-agent".into(),
                model_ref: "model-1".into(),
                instructions: Some("extract durable facts".into()),
                extraction_prompt: None,
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn extraction_intent_and_claim_survive_sqlite_reopen() {
        use awaken_session_contract::{
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
        assert_eq!(recovered, vec![claimed]);
        assert_eq!(recovered[0].status, MemoryExtractionStatus::Claimed);
        assert!(matches!(
            reopened
                .compare_and_swap_extraction(0, recovered[0].clone())
                .await,
            Err(awaken_session_contract::MemoryExtractionError::RevisionConflict(_))
        ));
    }

    #[tokio::test]
    async fn lifecycle_fact_survives_the_commit_to_notification_crash_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions-outbox.db");
        let path = path.to_string_lossy().to_string();
        {
            let repo = SqliteManagedSessionRepository::open(&path).unwrap();
            repo.save_owned_with_lifecycle(
                "ws_a",
                sample("sesn_tx"),
                fact("session:sesn_tx:created", "sesn_tx", "session.status_idled"),
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
    async fn terminal_state_and_its_fact_share_one_repository_commit() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        repo.save_owned_with_lifecycle(
            "ws_a",
            sample("sesn_terminal"),
            fact("created", "sesn_terminal", "session.status_idled"),
        )
        .await;
        repo.complete_lifecycle("created").await;

        repo.archive_with_lifecycle(
            "sesn_terminal",
            "2026-01-01T00:00:00Z",
            fact("terminated", "sesn_terminal", "session.status_terminated"),
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
        repo.delete_with_lifecycle(
            "sesn_terminal",
            fact("deleted", "sesn_terminal", "session.deleted"),
        )
        .await;
        assert_eq!(repo.get("sesn_terminal").await.unwrap().status, "deleted");
        assert_eq!(repo.pending_lifecycle().await[0].id, "deleted");
    }

    #[tokio::test]
    async fn round_trips_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let path = path.to_string_lossy().to_string();

        // First process: create + persist, then drop the repo (simulated exit).
        {
            let repo = SqliteManagedSessionRepository::open(&path).unwrap();
            repo.save(sample("sesn_1")).await;
            assert_eq!(repo.get("sesn_1").await, Some(sample("sesn_1")));
        }
        // Second process: a fresh repo over the same file restores the row.
        let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
        assert_eq!(
            reopened.get("sesn_1").await,
            Some(sample("sesn_1")),
            "the session config survives a restart"
        );
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
        repo.save(sample("sesn_1")).await;
        let mut updated = sample("sesn_1");
        updated.title = Some("Renamed".to_string());
        repo.save(updated.clone()).await;
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
            repo.save_owned("ws_a", sample("sesn_1")).await;
            assert_eq!(repo.owner("sesn_1").await, Some("ws_a".to_string()));
        }
        // After a restart the owner is still readable — the cross-process fence input
        // for the edge ownership guard (ADR-0051).
        let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
        assert_eq!(reopened.owner("sesn_1").await, Some("ws_a".to_string()));
        // A row saved but never owner-stamped defaults to the seeded scope.
        reopened.save(sample("sesn_2")).await;
        assert_eq!(reopened.owner("sesn_2").await, Some("default".to_string()));
        // An unknown session has no owner.
        assert_eq!(reopened.owner("sesn_missing").await, None);
    }

    /// A row whose JSON payload columns are corrupt (truncated write, manual edit,
    /// schema drift) must surface the decode error rather than silently folding to
    /// an empty `metadata`/`mcp_servers` — the old fail-open masked data loss on
    /// read. `decode` is now fallible and `get` propagates it via the module's
    /// `.expect` read-failure convention (the trait's `Option`-returning `get`
    /// cannot carry an error), so a corrupt row fails loudly like any unreadable row.
    #[tokio::test]
    #[should_panic(expected = "decode managed session")]
    async fn corrupt_json_columns_error_instead_of_decoding_to_defaults() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        repo.save(sample("sesn_1")).await;
        // Corrupt both JSON payload columns out-of-band (an on-disk corruption / drift).
        {
            let conn = repo.conn.lock().unwrap();
            conn.execute(
                "UPDATE managed_session \
                 SET metadata_json = ?2, mcp_json = ?3 WHERE session_id = ?1",
                params!["sesn_1", "{not valid json", "also-not-json"],
            )
            .unwrap();
        }
        // Reading the corrupt row now fails loudly (decode error surfaced) instead of
        // returning a hollow session with empty collections.
        let _ = repo.get("sesn_1").await;
    }

    /// Live Postgres round-trip, isolated in its own schema. Skips when no Postgres
    /// is reachable (`AWAKEN_TEST_DATABASE_URL`), proving the shared portable bundle
    /// and the same behavior on the network backend.
    #[tokio::test]
    async fn postgres_round_trips_and_upserts() {
        use awaken_session_contract::{MemoryExtractionRepository, PutMemoryExtractionOutcome};
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

        repo.save(sample("sesn_1")).await;
        assert_eq!(repo.get("sesn_1").await, Some(sample("sesn_1")));
        assert!(repo.get("sesn_missing").await.is_none());

        let mut updated = sample("sesn_1");
        updated.title = None; // exercises the nullable title column
        repo.save(updated.clone()).await;
        assert_eq!(repo.get("sesn_1").await, Some(updated));

        repo.save_owned_with_lifecycle(
            "ws_a",
            sample("sesn_pg_tx"),
            fact(
                "session:sesn_pg_tx:created",
                "sesn_pg_tx",
                "session.status_idled",
            ),
        )
        .await;
        assert_eq!(repo.owner("sesn_pg_tx").await.as_deref(), Some("ws_a"));
        assert_eq!(
            repo.pending_lifecycle().await[0].id,
            "session:sesn_pg_tx:created"
        );
        repo.complete_lifecycle("session:sesn_pg_tx:created").await;
        repo.archive_with_lifecycle(
            "sesn_pg_tx",
            "2026-07-19T00:00:00Z",
            fact(
                "session:sesn_pg_tx:terminated",
                "sesn_pg_tx",
                "session.status_terminated",
            ),
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
    }

    /// Postgres parity for the ADR-0051 owner `scope_id` — the same atomic
    /// `save_owned` / `owner` + default-seed assertions the SQLite test
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
        repo.save_owned("ws_a", sample("sesn_1")).await;
        assert_eq!(repo.owner("sesn_1").await, Some("ws_a".to_string()));

        // Second "process": a fresh pool over the same schema still reads the owner.
        let reopened = PostgresManagedSessionRepository::with_pool(pool().await)
            .await
            .expect("store");
        assert_eq!(reopened.owner("sesn_1").await, Some("ws_a".to_string()));
        // A row saved but never owner-stamped defaults to the seeded scope.
        reopened.save(sample("sesn_2")).await;
        assert_eq!(reopened.owner("sesn_2").await, Some("default".to_string()));
        // An unknown session has no owner.
        assert_eq!(reopened.owner("sesn_missing").await, None);
    }
}
