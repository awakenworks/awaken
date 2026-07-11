//! Postgres adapter (feature `postgres`, ADR-0043) for the admin-plane
//! aggregates, over the crate's own `admin` migration scope ([`admin_bundle`]):
//! one [`PostgresAdminStore`] serves the same four sync store ports the sqlite
//! backend does — [`InferenceProfileStore`], [`McpStore`] and
//! [`ResourceStore`] — from a single connection pool.
//!
//! The store ports are **sync and infallible** (a broken store is a
//! panic-worthy invariant violation, not a recoverable condition — same contract
//! as the in-memory and sqlite impls). `sqlx` is async-only, so this backend
//! owns a dedicated single-worker Tokio runtime and drives each short query to
//! completion on it via [`block`], which runs the future on a fresh OS thread so
//! it is safe to call from *inside* the server's request-handler runtime (a
//! nested `block_on` would panic otherwise). Each bridged future owns its inputs
//! (the pool is an `Arc`, cloned in; params are owned), so there are no
//! cross-thread borrow puzzles. Admin writes are low-frequency authoring
//! operations, so the per-call thread hop is negligible.

use std::future::Future;

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;
use tokio::runtime::{Builder, Handle, Runtime};

use awaken_config_resolver::{
    AgentMcpConfig, AgentResourceConfig, InferenceProfile, InferenceProfileStore, McpServerDef,
    McpStore, ResourceStore, WebhookEndpointDef, WebhookStore,
};

use crate::schema::admin_bundle;

/// The admin component's table namespace (its bundle prefix).
const NS: &str = "admin";

/// Errors from connecting or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("runtime: {0}")]
    Runtime(String),
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// Drive a future to completion on `handle`'s runtime, from a fresh OS thread so
/// this is safe to call from within another Tokio runtime (nesting `block_on`
/// panics). `make` builds the future *on the target thread*, so the future itself
/// never crosses a thread boundary and need not be `Send` (only the builder
/// closure and the output must be) — this sidesteps sqlx's
/// `Send`-not-general-enough puzzles.
fn block<T, F, Fut>(handle: &Handle, make: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T>,
    T: Send + 'static,
{
    let handle = handle.clone();
    std::thread::spawn(move || handle.block_on(make()))
        .join()
        .expect("admin store runtime thread panicked")
}

/// A Postgres-backed store for the admin-plane aggregates, implementing all four
/// sync store ports. Wrap in `Arc` and clone into each `AdminState` slot so the
/// ports share one pool.
pub struct PostgresAdminStore {
    pool: PgPool,
    handle: Handle,
    /// The store owns its runtime; `Option` so [`Drop`] can shut it down in the
    /// background (dropping a runtime from within another runtime would panic).
    rt: Option<Runtime>,
}

impl PostgresAdminStore {
    /// Connect and apply the admin migrations under the `admin` namespace. Sync so
    /// it composes with the sync ports; the pool lives on this store's own runtime.
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|err| StoreError::Runtime(err.to_string()))?;
        let handle = rt.handle().clone();
        let url = url.to_string();
        let pool = block(&handle, move || async move {
            let pool = PgPool::connect(&url)
                .await
                .map_err(|err| StoreError::Connect(err.to_string()))?;
            migrate(&pool).await?;
            Ok::<_, StoreError>(pool)
        })?;
        Ok(Self {
            pool,
            handle,
            rt: Some(rt),
        })
    }

    fn put_json<T: serde::Serialize>(&self, table: &str, key_col: &str, key: &str, value: &T) {
        let sql = format!(
            "INSERT INTO {NS}_{table} ({key_col}, data) VALUES ($1, $2) \
             ON CONFLICT ({key_col}) DO UPDATE SET data = excluded.data"
        );
        let pool = self.pool.clone();
        let key = key.to_string();
        let data = serde_json::to_value(value).expect("serialize admin row");
        block(&self.handle, move || async move {
            sqlx::query(&sql)
                .bind(key)
                .bind(Json(data))
                .execute(&pool)
                .await
                .expect("write admin row");
        });
    }

    fn get_json<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        table: &str,
        key_col: &str,
        key: &str,
    ) -> Option<T> {
        let sql = format!("SELECT data FROM {NS}_{table} WHERE {key_col} = $1");
        let pool = self.pool.clone();
        let key = key.to_string();
        block(&self.handle, move || async move {
            let row = sqlx::query(&sql)
                .bind(key)
                .fetch_optional(&pool)
                .await
                .expect("read admin row")?;
            let Json(value): Json<T> = row.try_get("data").expect("decode admin row");
            Some(value)
        })
    }

    /// All JSON rows of `table` ordered by `key_col` (the in-memory/sqlite stores'
    /// sorted-list contract).
    fn list_json<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        table: &str,
        key_col: &str,
    ) -> Vec<T> {
        let sql = format!("SELECT data FROM {NS}_{table} ORDER BY {key_col}");
        let pool = self.pool.clone();
        block(&self.handle, move || async move {
            let rows = sqlx::query(&sql)
                .fetch_all(&pool)
                .await
                .expect("list admin rows");
            rows.into_iter()
                .map(|row| {
                    let Json(value): Json<T> = row.try_get("data").expect("decode admin row");
                    value
                })
                .collect()
        })
    }

    /// Upsert an agent's resource binding (ADR-0038). Inherent counterpart of the
    /// sqlite store's method, matching the [`ResourceStore`] port.
    pub fn put_agent_resource(&self, config: AgentResourceConfig) {
        self.put_json(
            "agent_resource",
            "agent_id",
            &config.agent_id.clone(),
            &config,
        );
    }

    /// One agent's resource binding, `None` when the agent has none.
    pub fn get_agent_resource(&self, agent_id: &str) -> Option<AgentResourceConfig> {
        self.get_json("agent_resource", "agent_id", agent_id)
    }
}

impl Drop for PostgresAdminStore {
    fn drop(&mut self) {
        // A Tokio runtime must not be dropped from within another runtime (it would
        // panic on the blocking shutdown). `shutdown_background` hands the teardown
        // off so dropping the store from an async context (e.g. server shutdown) is
        // safe.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// Apply the `admin` migration bundle to `pool` (idempotent; the runner records
/// applied versions in `admin_schema_migrations`).
async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
    let bundle = admin_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|err| StoreError::Migrate(err.to_string()))?
        .run_bundle(&bundle)
        .await
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
    Ok(())
}

impl ResourceStore for PostgresAdminStore {
    fn put_agent_resource(&self, config: AgentResourceConfig) {
        // Fully-qualified so this resolves to the inherent method, not the trait one.
        PostgresAdminStore::put_agent_resource(self, config);
    }
    fn get_agent_resource(&self, agent_id: &str) -> Option<AgentResourceConfig> {
        PostgresAdminStore::get_agent_resource(self, agent_id)
    }
}

impl WebhookStore for PostgresAdminStore {
    fn put(&self, def: WebhookEndpointDef) {
        self.put_json("webhook", "id", &def.id.clone(), &def);
    }
    fn get(&self, id: &str) -> Option<WebhookEndpointDef> {
        self.get_json("webhook", "id", id)
    }
    fn list(&self, workspace_id: &str) -> Vec<WebhookEndpointDef> {
        // Reuse the sorted list bridge, then fence by owner in Rust (webhook rows are
        // low-cardinality; workspace lives inside the JSON, not a column).
        self.list_json::<WebhookEndpointDef>("webhook", "id")
            .into_iter()
            .filter(|d| d.workspace_id == workspace_id)
            .collect()
    }
    fn delete(&self, id: &str) -> bool {
        let sql = format!("DELETE FROM {NS}_webhook WHERE id = $1");
        let pool = self.pool.clone();
        let id = id.to_string();
        block(&self.handle, move || async move {
            sqlx::query(&sql)
                .bind(id)
                .execute(&pool)
                .await
                .expect("delete admin row")
                .rows_affected()
                > 0
        })
    }
}

impl InferenceProfileStore for PostgresAdminStore {
    fn put(&self, id: String, profile: InferenceProfile) {
        self.put_json("inference_profile", "id", &id, &profile);
    }
    fn get(&self, id: &str) -> Option<InferenceProfile> {
        self.get_json("inference_profile", "id", id)
    }
}

impl McpStore for PostgresAdminStore {
    fn put_server(&self, def: McpServerDef) {
        self.put_json("mcp_server", "id", &def.id.0.clone(), &def);
    }
    fn get_server(&self, id: &str) -> Option<McpServerDef> {
        self.get_json("mcp_server", "id", id)
    }
    fn list_servers(&self) -> Vec<McpServerDef> {
        self.list_json("mcp_server", "id")
    }
    fn put_agent_config(&self, config: AgentMcpConfig) {
        self.put_json("agent_mcp", "agent_id", &config.agent_id.clone(), &config);
    }
    fn get_agent_config(&self, agent_id: &str) -> Option<AgentMcpConfig> {
        self.get_json("agent_mcp", "agent_id", agent_id)
    }
}
