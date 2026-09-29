// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The product composition owns bounded connection acquisition and injects
// @formal reason: one handle per database/principal; this classification is not a proof claim.
//! PostgreSQL handles owned by one product-process composition.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPool};

type Database = (String, u16, Option<PathBuf>, String, String);

fn database(url: &str) -> Result<Database, String> {
    let options = PgConnectOptions::from_str(url)
        .map_err(|_| "invalid process PostgreSQL connection configuration".to_owned())?;
    Ok(database_options(&options))
}

fn database_options(options: &PgConnectOptions) -> Database {
    (
        options.get_host().to_owned(),
        options.get_port(),
        options.get_socket().cloned(),
        options.get_username().to_owned(),
        options
            .get_database()
            .unwrap_or(options.get_username())
            .to_owned(),
    )
}

/// This map exists only while assembling one process. It is not a global
/// registry: consuming adapters retain the existing SQLx pool handles.
pub(super) struct ProcessPostgresPools {
    maximum: u32,
    pools: BTreeMap<Database, (String, PgPool)>,
}

impl ProcessPostgresPools {
    pub(super) fn new(maximum: u32) -> Self {
        assert!(maximum > 0, "process pool capacity is positive");
        Self {
            maximum,
            pools: BTreeMap::new(),
        }
    }

    pub(super) fn seed(&mut self, url: &str, pool: PgPool) -> Result<(), String> {
        let key = database(url)?;
        if key != database_options(&pool.connect_options()) {
            return Err(
                "injected PostgreSQL pool targets a different database boundary".to_owned(),
            );
        }
        if self.pools.contains_key(&key) {
            return Err("process PostgreSQL pool was already supplied".to_owned());
        }
        if pool.options().get_max_connections() != self.maximum {
            return Err("injected PostgreSQL pool differs from the process capacity".to_owned());
        }
        self.pools.insert(key, (url.to_owned(), pool));
        Ok(())
    }

    pub(super) async fn get(&mut self, url: &str) -> Result<PgPool, String> {
        let key = database(url)?;
        if let Some((selected, pool)) = self.pools.get(&key) {
            // One tuple has one credential/TLS/session policy. A conflicting
            // trusted configuration is rejected rather than silently ignored
            // or interpreted as permission to create another pool.
            if selected != url {
                return Err("same PostgreSQL database and principal have conflicting connection configuration".to_owned());
            }
            return Ok(pool.clone());
        }
        let pool = awaken_sql_storage::postgres_pool_options()
            .max_connections(self.maximum)
            .connect(url)
            .await
            .map_err(|_| "connect process-owned PostgreSQL pool failed".to_owned())?;
        self.pools.insert(key, (url.to_owned(), pool.clone()));
        Ok(pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn injected_pool_is_reused_and_close_never_opens_a_fallback() {
        // P1 same tuple/configuration -> retain one supplied pool/capacity;
        // P2 closing it -> every clone is closed, no new connection attempt.
        // The invalid host makes any accidental fallback fail this rule.
        let url = "postgres://pool.invalid/database";
        let pool = awaken_sql_storage::postgres_pool_options()
            .max_connections(2)
            .connect_lazy(url)
            .unwrap();
        let mut pools = ProcessPostgresPools::new(2);
        pools.seed(url, pool.clone()).unwrap();
        let view = pools.get(url).await.unwrap();
        assert_eq!(view.options().get_max_connections(), 2);
        pool.close().await;
        assert!(view.is_closed());
        assert!(pools.get(url).await.unwrap().is_closed());
        assert_eq!(pools.pools.len(), 1);
    }

    #[tokio::test]
    async fn tuple_configuration_and_capacity_conflicts_fail_before_connection() {
        // P3 same endpoint/database/principal + different TLS/session policy
        // -> reject; P4 a duplicate supplied owner or wrong capacity -> reject.
        // No network or second owner is permitted by any rejection rule.
        let url = "postgres://pool.invalid/database";
        let pool = awaken_sql_storage::postgres_pool_options()
            .max_connections(2)
            .connect_lazy(url)
            .unwrap();
        let mut pools = ProcessPostgresPools::new(2);
        pools.seed(url, pool.clone()).unwrap();
        assert!(
            pools
                .get(&format!("{url}?application_name=other"))
                .await
                .is_err()
        );
        assert!(pools.seed(url, pool.clone()).is_err());
        assert!(
            ProcessPostgresPools::new(2)
                .seed("postgres://pool.invalid/another-database", pool.clone())
                .is_err()
        );
        assert!(ProcessPostgresPools::new(3).seed(url, pool).is_err());
        assert_eq!(pools.pools.len(), 1);
    }

    #[test]
    fn different_database_principal_or_endpoint_is_a_different_boundary() {
        // P5 varying exactly one tuple coordinate -> separate ownership;
        // invalid input -> explicit configuration failure. Schema and crate
        // identity are deliberately absent from the connection key.
        let original = database("postgres://first@pool.invalid/one").unwrap();
        for url in [
            "postgres://second@pool.invalid/one",
            "postgres://first@pool.invalid/two",
            "postgres://first@other.invalid/one",
            "postgres://first@pool.invalid:5433/one",
        ] {
            assert_ne!(original, database(url).unwrap());
        }
        assert!(database("not a PostgreSQL URI").is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires an explicit live PostgreSQL fixture for process pool cardinality"]
    async fn live_coordinator_adapters_share_the_process_connection_ceiling() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
        // P6 causes: a real PG schema, cap2, the canonical runtime pool supplied
        // before Coordinator/Resources/catalog/image adapters open. Effects:
        // all owned ledgers migrate/verify; every ordinary connection carries
        // the same tag; observed backend count <=2. P7 closing that pool makes
        // each verifier fail rather than opening an untagged fallback. Distinct
        // test schema and observer connection are outside the process budget.
        // Missing fixture is an explicit prerequisite, never a passing skip.
        let base = std::env::var("AWAKEN_TEST_DATABASE_URL")
            .expect("explicit live process-pool run requires AWAKEN_TEST_DATABASE_URL");
        use sqlx::ConnectOptions;
        let schema = format!("process_pool_{}", std::process::id());
        let tag = schema.clone();
        let admin = PgPool::connect(&base).await.unwrap();
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let options = PgConnectOptions::from_str(&base)
            .unwrap()
            .application_name(&tag)
            .options([("search_path", schema.as_str())]);
        let url = options.to_url_lossy().to_string();
        let pool = awaken_sql_storage::postgres_pool_options()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        let mut pools = ProcessPostgresPools::new(2);
        pools.seed(&url, pool.clone()).unwrap();
        let clock = awaken_lease_contract::system_clock();
        let resources = crate::resources::open_resources_application(
            crate::config::ResourceStoreBackend::Postgres(url.clone()),
            crate::PostgresSchemaMode::Migrate,
            clock.clone(),
            &mut pools,
        )
        .await
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let stores = crate::open_process_stores(crate::ProcessStoreOpenOptions {
            control: awaken_control::ControlStoreConfig::local(root.path()),
            coordinator: crate::config::CoordinatorStoreConfig {
                sessions: awaken_control::StoreBackend::Postgres(url.clone()),
                captured_content: awaken_control::StoreBackend::Postgres(url.clone()),
            },
            resources: Some(resources),
            postgres_pools: &mut pools,
            workspace_root: root.path().to_owned(),
            platform_workspace: None,
            publish_local_workspace: true,
            seal_key: None,
            role: crate::config::Role::Coordinator,
            postgres_schema: crate::PostgresSchemaMode::Migrate,
            clock,
        })
        .await
        .unwrap();
        let agents =
            std::sync::Arc::new(awaken_executable_agent_catalog::ExecutableAgentCatalog::new());
        let environments = std::sync::Arc::new(
            awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new(),
        );
        awaken_executable_agent_catalog::PostgresExecutableAgentRegistrar::with_pool(
            pool.clone(),
            agents.clone(),
        )
        .await
        .unwrap();
        awaken_executable_environment_catalog::PostgresExecutableEnvironmentRegistrar::with_pool(
            pool.clone(),
            environments.clone(),
        )
        .await
        .unwrap();
        awaken_environment_image_build::with_postgres_pool_environment_image_build_store(
            pool.clone(),
        )
        .await
        .unwrap();
        let first = pool.acquire().await.unwrap();
        let second = pool.acquire().await.unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE application_name = $1")
                .bind(&tag)
                .fetch_one(&admin)
                .await
                .unwrap();
        assert_eq!(count, 2, "P6 all adapters consume one ordinary cap");
        assert_eq!(pools.pools.len(), 1);
        drop(first);
        drop(second);
        pool.close().await;
        assert!(
            awaken_session_store::PostgresManagedSessionRepository::with_existing_pool(
                pool.clone()
            )
            .await
            .is_err()
        );
        assert!(
            awaken_work_store::PostgresWorkQueue::with_existing_pool(
                pool.clone(),
                awaken_lease_contract::system_clock()
            )
            .await
            .is_err()
        );
        assert!(awaken_coordinator::application_access_store::ApplicationAccessStore::with_existing_postgres_pool(pool.clone(), awaken_lease_contract::system_clock()).await.is_err());
        assert!(
            awaken_captured_content_store::PgCapturedContentStore::with_existing_pool(pool.clone())
                .await
                .is_err()
        );
        assert!(
            awaken_resource_persistence::open_postgres_with_pool(
                pool.clone(),
                awaken_resource_persistence::SchemaMode::Verify,
                None,
                awaken_lease_contract::system_clock()
            )
            .await
            .is_err()
        );
        assert!(
            awaken_executable_agent_catalog::PostgresExecutableAgentRegistrar::with_existing_pool(
                pool.clone(),
                agents
            )
            .await
            .is_err()
        );
        assert!(awaken_executable_environment_catalog::PostgresExecutableEnvironmentRegistrar::with_existing_pool(pool.clone(), environments).await.is_err());
        assert!(awaken_environment_image_build::with_existing_postgres_pool_environment_image_build_store(pool).await.is_err());
        drop(stores);
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await
            .unwrap();
            }).await.expect("whole process pool cardinality and closure rule finishes within 30s");
    }
}
