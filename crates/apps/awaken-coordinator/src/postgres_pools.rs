// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This private construction component selects the Coordinator-owned PostgreSQL
// @formal reason: pools under the existing storage contract and is not a formal-proof claim.
//! Shared PostgreSQL pool construction for Coordinator persistence compositions.

use awaken_deployment_config::{CoordinatorStorage, DispatchBackend, StoreKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PostgresComponents {
    pub(crate) dispatch: bool,
    pub(crate) commit: bool,
}

impl PostgresComponents {
    pub(crate) fn requires_process_pool(self) -> bool {
        self.dispatch || self.commit
    }

    pub(crate) fn exposes_committed_run_feeds(self) -> bool {
        self.commit
    }
}

pub(crate) fn postgres_components(storage: &CoordinatorStorage) -> PostgresComponents {
    PostgresComponents {
        dispatch: storage.dispatch_backend == DispatchBackend::Postgres,
        commit: storage.store == StoreKind::Postgres,
    }
}

pub(crate) fn database_url(storage: &CoordinatorStorage) -> Result<Option<&str>, String> {
    let components = postgres_components(storage);
    if !components.requires_process_pool() {
        return Ok(None);
    }
    storage
        .database_url
        .as_deref()
        .map(Some)
        .ok_or_else(|| "a Postgres Coordinator store requires runtime.database_url".to_string())
}

/// The process owns one authority pool and one dependent-read pool. Both use
/// the same primary URL, search path and checkout policy, with an independent
/// bounded quota. Migration-only composition continues to use one pool.
pub(crate) async fn open_postgres_pools(
    storage: &CoordinatorStorage,
) -> Result<(Option<sqlx::PgPool>, Option<sqlx::PgPool>), String> {
    let Some(url) = database_url(storage)? else {
        return Ok((None, None));
    };
    let options = awaken_sql_storage::postgres_pool_options()
        .max_connections(storage.postgres_max_connections.get());
    let pool = options
        .clone()
        .connect(url)
        .await
        .map_err(|error| format!("connect process-owned Coordinator Postgres pool: {error}"))?;
    let read_pool = options
        .connect(url)
        .await
        .map_err(|error| format!("connect Coordinator Postgres dependency read pool: {error}"))?;
    Ok((Some(pool), Some(read_pool)))
}
