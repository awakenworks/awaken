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
use awaken_protocol_managed::{ManagedSessionRepository, PersistedSession};
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
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
            // ADR-0048 D6: record the session's owning workspace (and, in cloud,
            // org) so webhooks/usage/audit project from the durable row. Nullable
            // so pre-owner rows migrate untouched (byte-identical read).
            Migration::new(
                2,
                "managed session owner: nullable workspace_id + org_id",
                "ALTER TABLE {prefix}_session ADD COLUMN workspace_id TEXT; \
                 ALTER TABLE {prefix}_session ADD COLUMN org_id TEXT",
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

#[allow(clippy::too_many_arguments)]
fn decode(
    session_id: String,
    agent_id: String,
    model: String,
    title: Option<String>,
    metadata_json: &str,
    environment_id: String,
    mcp_json: &str,
    workspace_id: Option<String>,
    org_id: Option<String>,
) -> PersistedSession {
    PersistedSession {
        session_id,
        agent_id,
        model,
        title,
        metadata: serde_json::from_str(metadata_json).unwrap_or_default(),
        environment_id,
        mcp_servers: serde_json::from_str(mcp_json).unwrap_or_default(),
        workspace_id,
        org_id,
    }
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
    async fn save(&self, session: PersistedSession) {
        let metadata_json = metadata_str(&session);
        let mcp_json = mcp_str(&session);
        let conn = self.conn.lock().expect("session store mutex poisoned");
        conn.execute(
            "INSERT INTO managed_session
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json,
                 workspace_id, org_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(session_id) DO UPDATE SET
                agent_id = excluded.agent_id,
                model = excluded.model,
                title = excluded.title,
                metadata_json = excluded.metadata_json,
                environment_id = excluded.environment_id,
                mcp_json = excluded.mcp_json,
                workspace_id = excluded.workspace_id,
                org_id = excluded.org_id",
            params![
                session.session_id,
                session.agent_id,
                session.model,
                session.title,
                metadata_json,
                session.environment_id,
                mcp_json,
                session.workspace_id,
                session.org_id,
            ],
        )
        .expect("persist managed session");
    }

    async fn get(&self, session_id: &str) -> Option<PersistedSession> {
        let conn = self.conn.lock().expect("session store mutex poisoned");
        conn.query_row(
            "SELECT agent_id, model, title, metadata_json, environment_id, mcp_json,
                    workspace_id, org_id
             FROM managed_session WHERE session_id = ?1",
            params![session_id],
            |row| {
                let metadata_json: String = row.get(3)?;
                let mcp_json: String = row.get(5)?;
                Ok(decode(
                    session_id.to_string(),
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    &metadata_json,
                    row.get(4)?,
                    &mcp_json,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()
        .expect("read managed session")
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
    async fn save(&self, session: PersistedSession) {
        sqlx::query(
            "INSERT INTO managed_session \
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json, \
                 workspace_id, org_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (session_id) DO UPDATE SET \
                agent_id = excluded.agent_id, \
                model = excluded.model, \
                title = excluded.title, \
                metadata_json = excluded.metadata_json, \
                environment_id = excluded.environment_id, \
                mcp_json = excluded.mcp_json, \
                workspace_id = excluded.workspace_id, \
                org_id = excluded.org_id",
        )
        .bind(&session.session_id)
        .bind(&session.agent_id)
        .bind(&session.model)
        .bind(&session.title)
        .bind(metadata_str(&session))
        .bind(&session.environment_id)
        .bind(mcp_str(&session))
        .bind(&session.workspace_id)
        .bind(&session.org_id)
        .execute(&self.pool)
        .await
        .expect("persist managed session");
    }

    async fn get(&self, session_id: &str) -> Option<PersistedSession> {
        let row = sqlx::query(
            "SELECT agent_id, model, title, metadata_json, environment_id, mcp_json, \
                    workspace_id, org_id \
             FROM managed_session WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .expect("read managed session")?;
        let metadata_json: String = row.get("metadata_json");
        let mcp_json: String = row.get("mcp_json");
        Some(decode(
            session_id.to_string(),
            row.get("agent_id"),
            row.get("model"),
            row.get("title"),
            &metadata_json,
            row.get("environment_id"),
            &mcp_json,
            row.get("workspace_id"),
            row.get("org_id"),
        ))
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
            workspace_id: Some("wrkspc_acme".to_string()),
            org_id: None,
        }
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

    /// ADR-0048 D6: the owning workspace is persisted and restored, and the bare
    /// pre-owner surface (`workspace_id: None`) round-trips as absent.
    #[tokio::test]
    async fn records_the_owning_workspace() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();

        let owned = sample("sesn_owned");
        assert_eq!(owned.workspace_id.as_deref(), Some("wrkspc_acme"));
        repo.save(owned.clone()).await;
        assert_eq!(
            repo.get("sesn_owned").await.and_then(|s| s.workspace_id),
            Some("wrkspc_acme".to_string()),
            "the owning workspace survives a reopen"
        );

        let mut bare = sample("sesn_bare");
        bare.workspace_id = None;
        repo.save(bare.clone()).await;
        let restored = repo.get("sesn_bare").await.unwrap();
        assert_eq!(restored.workspace_id, None, "bare surface stays owner-less");
        assert_eq!(restored, bare, "the full bare row round-trips");
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

    /// Live Postgres round-trip, isolated in its own schema. Skips when no Postgres
    /// is reachable (`AWAKEN_TEST_DATABASE_URL`), proving the shared portable bundle
    /// and the same behavior on the network backend.
    #[tokio::test]
    async fn postgres_round_trips_and_upserts() {
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
    }
}
