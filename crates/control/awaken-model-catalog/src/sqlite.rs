//! SQLite [`CatalogRepo`] adapter (feature `sqlite`, ADR-0043 sqlite-repos) over
//! the crate's own `catalog` migration scope ([`catalog_bundle`]). Rows are the
//! serde of the domain aggregates in the `data {json}` column; keyed columns
//! exist only for lookups. Same fail-closed semantics as the in-memory repo:
//! `put_endpoint` requires its provider, `put_offering` requires its endpoint and
//! re-validates the whole catalog inside a transaction (a rejected write leaves
//! no trace), and `snapshot()` runs [`ProviderCatalog::validate`] on the reload.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::repo::{CatalogRepo, RepoError};
use crate::schema::catalog_bundle;
use crate::{
    CatalogError, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderCatalog,
    ProviderId, ValidCatalog,
};

/// The catalog component's table namespace (its bundle prefix).
const NS: &str = "catalog";

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// A SQLite-backed [`CatalogRepo`].
pub struct SqliteCatalogRepo {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteCatalogRepo {
    /// Open (or create) a database file and apply the catalog migrations
    /// (one-step convenience for a store-owned database).
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let store =
            Self::over(Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database (tests / ephemeral).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let store = Self::over(
            Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?,
        );
        store.ensure_schema()?;
        Ok(store)
    }

    /// Wrap an existing connection **without migrating**. Call [`Self::ensure_schema`],
    /// or let a unified migration pipeline own the `catalog` scope so this store
    /// shares the caller's database.
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Apply the `catalog` scoped migration bundle (idempotent). Optional: skip it
    /// when the schema is owned externally.
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Migrate("catalog connection poisoned".to_string()))?;
        let bundle = catalog_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&conn, &bundle)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(())
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T, RepoError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &str) -> Result<T, RepoError> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| storage("catalog connection poisoned"))?;
            f(&mut guard, NS)
        })
        .await
        .map_err(storage)?
    }
}

/// A backend failure carried through `RepoError::Invariant(CatalogError::Storage)`
/// (see [`CatalogError::Storage`] for why it lives on the inner enum).
fn storage(err: impl std::fmt::Display) -> RepoError {
    RepoError::Invariant(CatalogError::Storage(err.to_string()))
}

fn row_exists(conn: &Connection, table: &str, id: &str) -> Result<bool, RepoError> {
    conn.query_row(
        &format!("SELECT 1 FROM {table} WHERE id = ?1"),
        params![id],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
    .map_err(storage)
}

/// Reload the full catalog projection from the tables. Offerings come back in a
/// deterministic order keyed on `(model_id, protocol_endpoint_id)` — the same
/// order the Postgres backend uses, so `resolve_offering`'s first-match is
/// reproducible and identical on both backends (portable, not rowid-dependent).
fn load_catalog(conn: &Connection, p: &str) -> Result<ProviderCatalog, RepoError> {
    let mut cat = ProviderCatalog::default();
    for data in select_data(conn, &format!("SELECT data FROM {p}_provider"))? {
        let provider: Provider = serde_json::from_str(&data).map_err(storage)?;
        cat.providers.insert(provider.id.0.clone(), provider);
    }
    for data in select_data(conn, &format!("SELECT data FROM {p}_protocol_endpoint"))? {
        let endpoint: ProtocolEndpoint = serde_json::from_str(&data).map_err(storage)?;
        cat.endpoints.insert(endpoint.id.0.clone(), endpoint);
    }
    for data in select_data(
        conn,
        &format!("SELECT data FROM {p}_offering ORDER BY model_id, protocol_endpoint_id"),
    )? {
        cat.offerings
            .push(serde_json::from_str(&data).map_err(storage)?);
    }
    Ok(cat)
}

fn select_data(conn: &Connection, sql: &str) -> Result<Vec<String>, RepoError> {
    let mut stmt = conn.prepare(sql).map_err(storage)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(storage)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(storage)
}

#[async_trait::async_trait]
impl CatalogRepo for SqliteCatalogRepo {
    async fn put_provider(&self, provider: Provider) -> Result<(), RepoError> {
        let id = provider.id.0.clone();
        let data = serde_json::to_string(&provider).map_err(storage)?;
        self.with_conn(move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT INTO {p}_provider (id, data) VALUES (?1, ?2) \
                     ON CONFLICT(id) DO UPDATE SET data = excluded.data"
                ),
                params![id, data],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn put_endpoint(&self, endpoint: ProtocolEndpoint) -> Result<(), RepoError> {
        let id = endpoint.id.0.clone();
        let provider_id = endpoint.provider_id.0.clone();
        let data = serde_json::to_string(&endpoint).map_err(storage)?;
        self.with_conn(move |conn, p| {
            if !row_exists(conn, &format!("{p}_provider"), &provider_id)? {
                return Err(RepoError::ProviderNotFound(provider_id));
            }
            conn.execute(
                &format!(
                    "INSERT INTO {p}_protocol_endpoint (id, provider_id, data) \
                     VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET \
                     provider_id = excluded.provider_id, data = excluded.data"
                ),
                params![id, provider_id, data],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn put_offering(&self, offering: Offering) -> Result<(), RepoError> {
        let model_id = offering.model_id.clone();
        let endpoint_id = offering.protocol_endpoint_id.0.clone();
        let data = serde_json::to_string(&offering).map_err(storage)?;
        self.with_conn(move |conn, p| {
            // Insert + whole-catalog re-validation in one transaction (fail-closed):
            // a rejected offering rolls back and leaves no trace — same no-trace
            // semantics as the in-memory repo's push-then-pop.
            let tx = conn.transaction().map_err(storage)?;
            if !row_exists(&tx, &format!("{p}_protocol_endpoint"), &endpoint_id)? {
                return Err(RepoError::EndpointNotFound(endpoint_id));
            }
            tx.execute(
                &format!(
                    "INSERT INTO {p}_offering (model_id, protocol_endpoint_id, data) \
                     VALUES (?1, ?2, ?3) ON CONFLICT(model_id, protocol_endpoint_id) \
                     DO UPDATE SET data = excluded.data"
                ),
                params![model_id, endpoint_id, data],
            )
            .map_err(storage)?;
            // Route the whole-catalog re-validation through the single construction
            // boundary (`ValidCatalog::parse`) — same fail-closed check, funneled.
            ValidCatalog::parse(load_catalog(&tx, p)?)?;
            tx.commit().map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn get_provider(&self, id: &ProviderId) -> Result<Provider, RepoError> {
        let id = id.0.clone();
        self.with_conn(move |conn, p| {
            let data: Option<String> = conn
                .query_row(
                    &format!("SELECT data FROM {p}_provider WHERE id = ?1"),
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage)?;
            let data = data.ok_or(RepoError::ProviderNotFound(id))?;
            serde_json::from_str(&data).map_err(storage)
        })
        .await
    }

    async fn get_endpoint(&self, id: &ProtocolEndpointId) -> Result<ProtocolEndpoint, RepoError> {
        let id = id.0.clone();
        self.with_conn(move |conn, p| {
            let data: Option<String> = conn
                .query_row(
                    &format!("SELECT data FROM {p}_protocol_endpoint WHERE id = ?1"),
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage)?;
            let data = data.ok_or(RepoError::EndpointNotFound(id))?;
            serde_json::from_str(&data).map_err(storage)
        })
        .await
    }

    async fn snapshot(&self) -> Result<ProviderCatalog, RepoError> {
        self.with_conn(move |conn, p| {
            // Load-time integrity guard against corrupt rows, funneled through the
            // same `ValidCatalog::parse` boundary; hand back the checked inner.
            Ok(ValidCatalog::parse(load_catalog(conn, p)?)?.into_inner())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The injection seam: a caller supplies its own connection, `over` wraps it
    /// WITHOUT migrating, and `ensure_schema` applies the `catalog` scope — so a
    /// unified migration pipeline can own the schema and this store shares the
    /// caller's database (mirrors the resource-family stores' `over`/`ensure_schema`).
    #[tokio::test]
    async fn over_then_ensure_schema_round_trips_on_a_caller_owned_connection() {
        let conn = Connection::open_in_memory().unwrap();
        let repo = SqliteCatalogRepo::over(conn);
        repo.ensure_schema().unwrap();
        repo.put_provider(Provider {
            id: ProviderId::new("anthropic"),
            slug: "anthropic".into(),
            display_name: "Anthropic".into(),
            version: 1,
        })
        .await
        .unwrap();
        assert_eq!(
            repo.get_provider(&ProviderId::new("anthropic"))
                .await
                .unwrap()
                .slug,
            "anthropic"
        );
    }
}
