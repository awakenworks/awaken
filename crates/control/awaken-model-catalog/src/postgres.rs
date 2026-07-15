//! Postgres [`CatalogRepo`] adapter (feature `postgres`, ADR-0043) over the
//! crate's own `catalog` migration scope ([`catalog_bundle`]) — the network-DB
//! sibling of [`SqliteCatalogRepo`](crate::sqlite::SqliteCatalogRepo). Rows are the
//! serde of the domain aggregates in the `data {json}` (jsonb) column; keyed
//! columns exist only for lookups. Same fail-closed semantics as the other
//! backends: `put_endpoint` requires its provider, `put_offering` requires its
//! endpoint and re-validates the whole catalog inside a transaction (a rejected
//! write leaves no trace), and `snapshot()` runs [`ProviderCatalog::validate`] on
//! the reload.

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use crate::repo::{CatalogRepo, RepoError};
use crate::schema::catalog_bundle;
use crate::{
    CatalogError, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderCatalog,
    ProviderId, ValidCatalog,
};

/// The catalog component's table namespace (its bundle prefix).
const NS: &str = "catalog";

/// Errors from connecting or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// A Postgres-backed [`CatalogRepo`].
pub struct PostgresCatalogRepo {
    pool: PgPool,
}

impl PostgresCatalogRepo {
    /// Connect and apply the catalog migrations under the `catalog` namespace.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool: apply the catalog migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        let bundle = catalog_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&bundle)
            .await
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(Self { pool })
    }
}

/// A backend failure carried through `RepoError::Invariant(CatalogError::Storage)`.
fn storage(err: impl std::fmt::Display) -> RepoError {
    RepoError::Invariant(CatalogError::Storage(err.to_string()))
}

/// Reload the full catalog projection from the tables. Offerings come back in a
/// deterministic order keyed on `(model_id, protocol_endpoint_id)` — identical to
/// the SQLite backend and stable across reopens/upserts (portable Postgres has no
/// implicit rowid), so `resolve_offering`'s first-match is reproducible on every
/// backend. `exec` is any Postgres executor (pool or transaction).
async fn load_catalog(
    conn: &mut sqlx::PgConnection,
    p: &str,
) -> Result<ProviderCatalog, RepoError> {
    let mut cat = ProviderCatalog::default();
    for row in sqlx::query(&format!("SELECT data FROM {p}_provider"))
        .fetch_all(&mut *conn)
        .await
        .map_err(storage)?
    {
        let Json(provider): Json<Provider> = row.try_get("data").map_err(storage)?;
        cat.providers.insert(provider.id.0.clone(), provider);
    }
    for row in sqlx::query(&format!("SELECT data FROM {p}_protocol_endpoint"))
        .fetch_all(&mut *conn)
        .await
        .map_err(storage)?
    {
        let Json(endpoint): Json<ProtocolEndpoint> = row.try_get("data").map_err(storage)?;
        cat.endpoints.insert(endpoint.id.0.clone(), endpoint);
    }
    for row in sqlx::query(&format!(
        "SELECT data FROM {p}_offering ORDER BY model_id, protocol_endpoint_id"
    ))
    .fetch_all(&mut *conn)
    .await
    .map_err(storage)?
    {
        let Json(offering): Json<Offering> = row.try_get("data").map_err(storage)?;
        cat.offerings.push(offering);
    }
    Ok(cat)
}

async fn row_exists<'e, E>(exec: E, table: &str, id: &str) -> Result<bool, RepoError>
where
    E: sqlx::PgExecutor<'e>,
{
    let found: Option<i32> = sqlx::query_scalar(&format!("SELECT 1 FROM {table} WHERE id = $1"))
        .bind(id)
        .fetch_optional(exec)
        .await
        .map_err(storage)?;
    Ok(found.is_some())
}

#[async_trait::async_trait]
impl CatalogRepo for PostgresCatalogRepo {
    async fn put_provider(&self, provider: Provider) -> Result<(), RepoError> {
        let p = NS;
        sqlx::query(&format!(
            "INSERT INTO {p}_provider (id, data) VALUES ($1, $2) \
             ON CONFLICT (id) DO UPDATE SET data = excluded.data"
        ))
        .bind(&provider.id.0)
        .bind(Json(&provider))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn put_endpoint(&self, endpoint: ProtocolEndpoint) -> Result<(), RepoError> {
        let p = NS;
        if !row_exists(
            &self.pool,
            &format!("{p}_provider"),
            &endpoint.provider_id.0,
        )
        .await?
        {
            return Err(RepoError::ProviderNotFound(endpoint.provider_id.0.clone()));
        }
        sqlx::query(&format!(
            "INSERT INTO {p}_protocol_endpoint (id, provider_id, data) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET \
             provider_id = excluded.provider_id, data = excluded.data"
        ))
        .bind(&endpoint.id.0)
        .bind(&endpoint.provider_id.0)
        .bind(Json(&endpoint))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn put_offering(&self, offering: Offering) -> Result<(), RepoError> {
        let p = NS;
        // Insert + whole-catalog re-validation in one transaction (fail-closed): a
        // rejected offering rolls back and leaves no trace — same no-trace semantics
        // as the in-memory repo's push-then-pop.
        let mut tx = self.pool.begin().await.map_err(storage)?;
        if !row_exists(
            &mut *tx,
            &format!("{p}_protocol_endpoint"),
            &offering.protocol_endpoint_id.0,
        )
        .await?
        {
            return Err(RepoError::EndpointNotFound(
                offering.protocol_endpoint_id.0.clone(),
            ));
        }
        sqlx::query(&format!(
            "INSERT INTO {p}_offering (model_id, protocol_endpoint_id, data) \
             VALUES ($1, $2, $3) ON CONFLICT (model_id, protocol_endpoint_id) \
             DO UPDATE SET data = excluded.data"
        ))
        .bind(&offering.model_id)
        .bind(&offering.protocol_endpoint_id.0)
        .bind(Json(&offering))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        // Route the whole-catalog re-validation through the single construction
        // boundary (`ValidCatalog::parse`) — same fail-closed check, funneled.
        ValidCatalog::parse(load_catalog(&mut tx, p).await?)?; // Transaction derefs to PgConnection
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn get_provider(&self, id: &ProviderId) -> Result<Provider, RepoError> {
        let p = NS;
        let row = sqlx::query(&format!("SELECT data FROM {p}_provider WHERE id = $1"))
            .bind(&id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
        let row = row.ok_or_else(|| RepoError::ProviderNotFound(id.0.clone()))?;
        let Json(provider): Json<Provider> = row.try_get("data").map_err(storage)?;
        Ok(provider)
    }

    async fn get_endpoint(&self, id: &ProtocolEndpointId) -> Result<ProtocolEndpoint, RepoError> {
        let p = NS;
        let row = sqlx::query(&format!(
            "SELECT data FROM {p}_protocol_endpoint WHERE id = $1"
        ))
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let row = row.ok_or_else(|| RepoError::EndpointNotFound(id.0.clone()))?;
        let Json(endpoint): Json<ProtocolEndpoint> = row.try_get("data").map_err(storage)?;
        Ok(endpoint)
    }

    async fn snapshot(&self) -> Result<ProviderCatalog, RepoError> {
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        // Load-time integrity guard against corrupt rows, funneled through the same
        // `ValidCatalog::parse` boundary; hand back the checked inner.
        let cat = load_catalog(&mut conn, NS).await?;
        Ok(ValidCatalog::parse(cat)?.into_inner())
    }
}
