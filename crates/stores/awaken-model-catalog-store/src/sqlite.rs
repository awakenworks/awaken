// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This source implements the repository-controlled store product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! SQLite [`CatalogRepo`] adapter (feature `sqlite`, ADR-0043 sqlite-repos) over
//! the crate's own `catalog` migration scope ([`catalog_bundle`]). Rows are the
//! serde of the domain aggregates in the `data {json}` column; keyed columns
//! exist only for lookups. Same fail-closed semantics as the in-memory repo:
//! `put_endpoint` requires its provider, `put_offering` requires its endpoint and
//! re-validates the whole catalog inside a transaction (a rejected write leaves
//! no trace), and `snapshot()` runs [`ProviderCatalog::validate`] on the reload.

use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;
use sqlx::SqliteConnection;

use crate::schema::catalog_bundle;
use awaken_model_catalog::repo::{CatalogRepo, RepoError};
use awaken_model_catalog::{
    BrokeredCatalogProjection, CatalogError, CatalogSyncResult, DiscoveredModel, ModelAttributes,
    Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderCatalog, ProviderId,
    ValidCatalog,
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
    pool: EmbeddedPool,
}

impl SqliteCatalogRepo {
    /// Open (or create) a database file and apply the catalog migrations
    /// (one-step convenience for a store-owned database).
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        Self::open_at(EmbeddedSqlite::file(path)).await
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_at(EmbeddedSqlite::memory()).await
    }

    async fn open_at(location: EmbeddedSqlite) -> Result<Self, StoreError> {
        let store = Self::over(
            location
                .open()
                .await
                .map_err(|err| StoreError::Open(err.to_string()))?,
        );
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an existing pool **without migrating**. Call [`Self::ensure_schema`],
    /// or let a unified migration pipeline own the `catalog` scope so this store
    /// shares the caller's database.
    pub fn over(pool: EmbeddedPool) -> Self {
        Self { pool }
    }

    /// Apply the `catalog` scoped migration bundle (idempotent). Optional: skip it
    /// when the schema is owned externally.
    pub async fn ensure_schema(&self) -> Result<(), StoreError> {
        let bundle = catalog_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_sqlite_pool::held(
            SqliteMigrationRunner::with_prefix(self.pool.sqlx().clone(), NS)
                .map_err(|err| StoreError::Migrate(err.to_string()))?
                .run_bundle(&bundle),
        )
        .await
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(())
    }
}

/// A backend failure carried through `RepoError::Invariant(CatalogError::Storage)`
/// (see [`CatalogError::Storage`] for why it lives on the inner enum).
fn storage(err: impl std::fmt::Display) -> RepoError {
    RepoError::Invariant(CatalogError::Storage(err.to_string()))
}

async fn row_exists(conn: &mut SqliteConnection, table: &str, id: &str) -> Result<bool, RepoError> {
    sqlx::query_scalar::<_, i64>(&format!("SELECT 1 FROM {table} WHERE id = ?1"))
        .bind(id)
        .fetch_optional(conn)
        .await
        .map(|found| found.is_some())
        .map_err(storage)
}

/// Reload the full catalog projection from the tables. Offerings come back in a
/// deterministic order keyed on `(model_id, protocol_endpoint_id)` — the same
/// order the Postgres backend uses, so `resolve_offering`'s first-match is
/// reproducible and identical on both backends (portable, not rowid-dependent).
async fn load_catalog(conn: &mut SqliteConnection) -> Result<ProviderCatalog, RepoError> {
    let mut cat = ProviderCatalog::default();
    for data in select_data(&mut *conn, &format!("SELECT data FROM {NS}_provider")).await? {
        let provider: Provider = serde_json::from_str(&data).map_err(storage)?;
        cat.providers.insert(provider.id.0.clone(), provider);
    }
    for data in select_data(
        &mut *conn,
        &format!("SELECT data FROM {NS}_protocol_endpoint"),
    )
    .await?
    {
        let endpoint: ProtocolEndpoint = serde_json::from_str(&data).map_err(storage)?;
        cat.endpoints.insert(endpoint.id.0.clone(), endpoint);
    }
    for data in select_data(
        &mut *conn,
        &format!("SELECT data FROM {NS}_offering ORDER BY model_id, protocol_endpoint_id"),
    )
    .await?
    {
        cat.offerings
            .push(serde_json::from_str(&data).map_err(storage)?);
    }
    for (model_id, data) in select_keyed(
        &mut *conn,
        &format!("SELECT model_id, data FROM {NS}_model_attributes"),
    )
    .await?
    {
        let attrs: ModelAttributes = serde_json::from_str(&data).map_err(storage)?;
        cat.model_attributes.insert(model_id, attrs);
    }
    Ok(cat)
}

async fn select_data(conn: &mut SqliteConnection, sql: &str) -> Result<Vec<String>, RepoError> {
    sqlx::query_scalar::<_, String>(sql)
        .fetch_all(conn)
        .await
        .map_err(storage)
}

/// Like [`select_data`] but reads a `(key, data)` pair per row — for tables keyed
/// on their own id column (e.g. `_model_attributes` keyed on `model_id`).
async fn select_keyed(
    conn: &mut SqliteConnection,
    sql: &str,
) -> Result<Vec<(String, String)>, RepoError> {
    sqlx::query_as::<_, (String, String)>(sql)
        .fetch_all(conn)
        .await
        .map_err(storage)
}

async fn upsert_provider(
    conn: &mut SqliteConnection,
    id: &str,
    data: &str,
) -> Result<(), RepoError> {
    sqlx::query(&format!(
        "INSERT INTO {NS}_provider (id, data) VALUES (?1, ?2) \
         ON CONFLICT(id) DO UPDATE SET data = excluded.data"
    ))
    .bind(id)
    .bind(data)
    .execute(conn)
    .await
    .map_err(storage)?;
    Ok(())
}

async fn upsert_endpoint(
    conn: &mut SqliteConnection,
    id: &str,
    provider_id: &str,
    data: &str,
) -> Result<(), RepoError> {
    sqlx::query(&format!(
        "INSERT INTO {NS}_protocol_endpoint (id, provider_id, data) \
         VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET \
         provider_id = excluded.provider_id, data = excluded.data"
    ))
    .bind(id)
    .bind(provider_id)
    .bind(data)
    .execute(conn)
    .await
    .map_err(storage)?;
    Ok(())
}

async fn upsert_offering(
    conn: &mut SqliteConnection,
    model_id: &str,
    endpoint_id: &str,
    data: &str,
) -> Result<(), RepoError> {
    sqlx::query(&format!(
        "INSERT INTO {NS}_offering (model_id, protocol_endpoint_id, data) \
         VALUES (?1, ?2, ?3) ON CONFLICT(model_id, protocol_endpoint_id) \
         DO UPDATE SET data = excluded.data"
    ))
    .bind(model_id)
    .bind(endpoint_id)
    .bind(data)
    .execute(conn)
    .await
    .map_err(storage)?;
    Ok(())
}

#[async_trait::async_trait]
impl CatalogRepo for SqliteCatalogRepo {
    async fn put_discovered_connection(
        &self,
        provider: Provider,
        endpoint: ProtocolEndpoint,
        models: Vec<DiscoveredModel>,
        observed_at_unix_ms: u64,
    ) -> Result<CatalogSyncResult, RepoError> {
        if endpoint.provider_id != provider.id {
            return Err(RepoError::ProviderNotFound(endpoint.provider_id.0));
        }
        let provider_id = provider.id.0.clone();
        let endpoint_id = endpoint.id.0.clone();
        let provider_data = serde_json::to_string(&provider).map_err(storage)?;
        let endpoint_data = serde_json::to_string(&endpoint).map_err(storage)?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        upsert_provider(&mut tx, &provider_id, &provider_data).await?;
        upsert_endpoint(&mut tx, &endpoint_id, &provider_id, &endpoint_data).await?;
        let mut catalog = load_catalog(&mut tx).await?;
        let result = catalog.reconcile_discovered_models(
            &ProtocolEndpointId::new(endpoint_id.clone()),
            models,
            observed_at_unix_ms,
        )?;
        for offering in catalog
            .offerings
            .iter()
            .filter(|offering| offering.protocol_endpoint_id.as_str() == endpoint_id)
        {
            let data = serde_json::to_string(offering).map_err(storage)?;
            upsert_offering(&mut tx, &offering.model_id, &endpoint_id, &data).await?;
        }
        ValidCatalog::parse(load_catalog(&mut tx).await?)?;
        tx.commit().await.map_err(storage)?;
        Ok(result)
    }

    async fn put_provider(&self, provider: Provider) -> Result<(), RepoError> {
        let data = serde_json::to_string(&provider).map_err(storage)?;
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        upsert_provider(&mut conn, &provider.id.0, &data).await
    }

    async fn put_endpoint(&self, endpoint: ProtocolEndpoint) -> Result<(), RepoError> {
        let provider_id = endpoint.provider_id.0.clone();
        let data = serde_json::to_string(&endpoint).map_err(storage)?;
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        if !row_exists(&mut conn, &format!("{NS}_provider"), &provider_id).await? {
            return Err(RepoError::ProviderNotFound(provider_id));
        }
        upsert_endpoint(&mut conn, &endpoint.id.0, &provider_id, &data).await
    }

    async fn put_offering(&self, mut offering: Offering) -> Result<(), RepoError> {
        offering.source = awaken_model_catalog::OfferingSource::Manual;
        offering.status = awaken_model_catalog::OfferingStatus::Active;
        offering.last_seen_at_unix_ms = None;
        let endpoint_id = offering.protocol_endpoint_id.0.clone();
        let data = serde_json::to_string(&offering).map_err(storage)?;
        // Insert + whole-catalog re-validation in one transaction (fail-closed):
        // a rejected offering rolls back and leaves no trace — same no-trace
        // semantics as the in-memory repo's push-then-pop.
        let mut tx = self.pool.begin().await.map_err(storage)?;
        if !row_exists(&mut tx, &format!("{NS}_protocol_endpoint"), &endpoint_id).await? {
            return Err(RepoError::EndpointNotFound(endpoint_id));
        }
        upsert_offering(&mut tx, &offering.model_id, &endpoint_id, &data).await?;
        // Route the whole-catalog re-validation through the single construction
        // boundary (`ValidCatalog::parse`) — same fail-closed check, funneled.
        ValidCatalog::parse(load_catalog(&mut tx).await?)?;
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn reconcile_discovered_models(
        &self,
        endpoint_id: &ProtocolEndpointId,
        models: Vec<DiscoveredModel>,
        observed_at_unix_ms: u64,
    ) -> Result<CatalogSyncResult, RepoError> {
        let endpoint_id = endpoint_id.0.clone();
        let mut tx = self.pool.begin().await.map_err(storage)?;
        if !row_exists(&mut tx, &format!("{NS}_protocol_endpoint"), &endpoint_id).await? {
            return Err(RepoError::EndpointNotFound(endpoint_id));
        }
        let mut catalog = load_catalog(&mut tx).await?;
        let result = catalog.reconcile_discovered_models(
            &ProtocolEndpointId::new(endpoint_id.clone()),
            models,
            observed_at_unix_ms,
        )?;
        for offering in catalog
            .offerings
            .iter()
            .filter(|offering| offering.protocol_endpoint_id.as_str() == endpoint_id)
        {
            let data = serde_json::to_string(offering).map_err(storage)?;
            upsert_offering(&mut tx, &offering.model_id, &endpoint_id, &data).await?;
        }
        ValidCatalog::parse(load_catalog(&mut tx).await?)?;
        tx.commit().await.map_err(storage)?;
        Ok(result)
    }

    async fn reconcile_brokered_projection(
        &self,
        projection: BrokeredCatalogProjection,
    ) -> Result<CatalogSyncResult, RepoError> {
        let endpoint_prefix = format!("brokered:{}:", projection.broker_id.trim());
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let mut catalog = load_catalog(&mut tx).await?;
        let result = catalog.reconcile_brokered_projection(projection)?;
        for provider in catalog.providers.values() {
            let data = serde_json::to_string(provider).map_err(storage)?;
            upsert_provider(&mut tx, &provider.id.0, &data).await?;
        }
        for endpoint in catalog
            .endpoints
            .values()
            .filter(|endpoint| endpoint.id.0.starts_with(&endpoint_prefix))
        {
            let data = serde_json::to_string(endpoint).map_err(storage)?;
            upsert_endpoint(&mut tx, &endpoint.id.0, &endpoint.provider_id.0, &data).await?;
        }
        for offering in catalog.offerings.iter().filter(|offering| {
            offering.source == awaken_model_catalog::OfferingSource::Brokered
                && offering
                    .protocol_endpoint_id
                    .0
                    .starts_with(&endpoint_prefix)
        }) {
            let data = serde_json::to_string(offering).map_err(storage)?;
            upsert_offering(
                &mut tx,
                &offering.model_id,
                &offering.protocol_endpoint_id.0,
                &data,
            )
            .await?;
        }
        // The aggregate has already merged Brokered observations with any
        // authoritative local facts. Persist that complete attribute view in
        // the same transaction so stale Brokered-only values disappear while
        // Manual/Provider API values survive the refresh.
        sqlx::query(&format!("DELETE FROM {NS}_model_attributes"))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        for (model_id, attributes) in &catalog.model_attributes {
            let data = serde_json::to_string(attributes).map_err(storage)?;
            sqlx::query(&format!(
                "INSERT INTO {NS}_model_attributes (model_id, data) VALUES (?1, ?2)"
            ))
            .bind(model_id)
            .bind(&data)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        ValidCatalog::parse(load_catalog(&mut tx).await?)?;
        tx.commit().await.map_err(storage)?;
        Ok(result)
    }

    async fn put_model_attributes(
        &self,
        model_id: String,
        attrs: ModelAttributes,
    ) -> Result<(), RepoError> {
        attrs.validate(&model_id)?;
        let data = serde_json::to_string(&attrs).map_err(storage)?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_model_attributes (model_id, data) VALUES (?1, ?2) \
             ON CONFLICT(model_id) DO UPDATE SET data = excluded.data"
        ))
        .bind(&model_id)
        .bind(&data)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn get_provider(&self, id: &ProviderId) -> Result<Provider, RepoError> {
        let data = sqlx::query_scalar::<_, String>(&format!(
            "SELECT data FROM {NS}_provider WHERE id = ?1"
        ))
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let data = data.ok_or_else(|| RepoError::ProviderNotFound(id.0.clone()))?;
        serde_json::from_str(&data).map_err(storage)
    }

    async fn get_endpoint(&self, id: &ProtocolEndpointId) -> Result<ProtocolEndpoint, RepoError> {
        let data = sqlx::query_scalar::<_, String>(&format!(
            "SELECT data FROM {NS}_protocol_endpoint WHERE id = ?1"
        ))
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let data = data.ok_or_else(|| RepoError::EndpointNotFound(id.0.clone()))?;
        serde_json::from_str(&data).map_err(storage)
    }

    async fn snapshot(&self) -> Result<ProviderCatalog, RepoError> {
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        // Load-time integrity guard against corrupt rows, funneled through the
        // same `ValidCatalog::parse` boundary; hand back the checked inner.
        Ok(ValidCatalog::parse(load_catalog(&mut conn).await?)?.into_inner())
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
        let pool = EmbeddedSqlite::memory().open().await.unwrap();
        let repo = SqliteCatalogRepo::over(pool);
        repo.ensure_schema().await.unwrap();
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

    /// Model attributes publish independently of offerings (no provider/endpoint
    /// reference) and round-trip through the durable table into the snapshot.
    #[tokio::test]
    async fn put_model_attributes_round_trips_through_the_durable_snapshot() {
        let repo = SqliteCatalogRepo::open_in_memory().await.unwrap();
        repo.put_model_attributes(
            "claude-opus-4-8".into(),
            ModelAttributes {
                context_window: Some(200_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let snap = repo.snapshot().await.unwrap();
        assert_eq!(
            snap.model_attributes["claude-opus-4-8"].context_window,
            Some(200_000)
        );
    }
}
