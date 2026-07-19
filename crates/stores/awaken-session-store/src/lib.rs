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
use awaken_session_contract::{ManagedSessionRepository, PersistedSession};

// The in-memory reference backends (plain + scoped) live here beside the durable
// siblings (issue A / Phase 1); the ports + PersistedSession value + the
// ScopedSessionRepo decorator stay inward in `awaken-session-contract`.
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
        ],
    )
}

fn metadata_str(session: &PersistedSession) -> String {
    serde_json::to_string(&session.metadata).expect("session metadata serializes")
}

fn mcp_str(session: &PersistedSession) -> String {
    serde_json::to_string(&session.mcp_servers).expect("session mcp servers serialize")
}

/// Decode a persisted row's JSON payload columns. A corrupt column (truncated
/// write, manual edit, schema drift) surfaces as `Err` rather than silently
/// folding to an empty `metadata`/`mcp_servers` — that fail-open masked data loss
/// on read. Callers within this module treat it like any other unreadable row
/// (the module's `.expect` convention for read failures), so a corrupt row fails
/// loudly instead of returning a hollow session.
fn decode(
    session_id: String,
    agent_id: String,
    model: String,
    title: Option<String>,
    metadata_json: &str,
    environment_id: String,
    mcp_json: &str,
) -> Result<PersistedSession, serde_json::Error> {
    Ok(PersistedSession {
        session_id,
        agent_id,
        model,
        title,
        metadata: serde_json::from_str(metadata_json)?,
        environment_id,
        mcp_servers: serde_json::from_str(mcp_json)?,
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
        let conn = self.conn.lock().expect("session store mutex poisoned");
        conn.execute(
            "INSERT INTO managed_session
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json, scope_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(session_id) DO UPDATE SET
                agent_id = excluded.agent_id,
                model = excluded.model,
                title = excluded.title,
                metadata_json = excluded.metadata_json,
                environment_id = excluded.environment_id,
                mcp_json = excluded.mcp_json,
                scope_id = excluded.scope_id",
            params![
                session.session_id,
                session.agent_id,
                session.model,
                session.title,
                metadata_json,
                session.environment_id,
                mcp_json,
                owner_scope,
            ],
        )
        .expect("persist managed session");
    }

    async fn get(&self, session_id: &str) -> Option<PersistedSession> {
        let conn = self.conn.lock().expect("session store mutex poisoned");
        let raw = conn
            .query_row(
                "SELECT agent_id, model, title, metadata_json, environment_id, mcp_json
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
                    ))
                },
            )
            .optional()
            .expect("read managed session")?;
        let (agent_id, model, title, metadata_json, environment_id, mcp_json) = raw;
        Some(
            decode(
                session_id.to_string(),
                agent_id,
                model,
                title,
                &metadata_json,
                environment_id,
                &mcp_json,
            )
            .expect("decode managed session"),
        )
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
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json, scope_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (session_id) DO UPDATE SET \
                agent_id = excluded.agent_id, \
                model = excluded.model, \
                title = excluded.title, \
                metadata_json = excluded.metadata_json, \
                environment_id = excluded.environment_id, \
                mcp_json = excluded.mcp_json, \
                scope_id = excluded.scope_id",
        )
        .bind(&session.session_id)
        .bind(&session.agent_id)
        .bind(&session.model)
        .bind(&session.title)
        .bind(metadata_str(&session))
        .bind(&session.environment_id)
        .bind(mcp_str(&session))
        .bind(owner_scope)
        .execute(&self.pool)
        .await
        .expect("persist managed session");
    }

    async fn get(&self, session_id: &str) -> Option<PersistedSession> {
        let row = sqlx::query(
            "SELECT agent_id, model, title, metadata_json, environment_id, mcp_json \
             FROM managed_session WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .expect("read managed session")?;
        let metadata_json: String = row.get("metadata_json");
        let mcp_json: String = row.get("mcp_json");
        Some(
            decode(
                session_id.to_string(),
                row.get("agent_id"),
                row.get("model"),
                row.get("title"),
                &metadata_json,
                row.get("environment_id"),
                &mcp_json,
            )
            .expect("decode managed session"),
        )
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
