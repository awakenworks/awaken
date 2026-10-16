//! Process adapter for the canonical Resources persistence selection.

use super::ProcessResources;
use crate::{PostgresSchemaMode, config};

impl ProcessResources {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn ephemeral(
        clock: std::sync::Arc<dyn awaken_time::WallClock>,
    ) -> ProcessResources {
        let invalidator = std::sync::Arc::new(awaken_sandbox_memoryd::LocalInvalidator::default());
        let application = awaken_resource_persistence::ephemeral(clock, invalidator.clone())
            .await
            .expect("open ephemeral Resources application");
        ProcessResources {
            application,
            invalidator,
        }
    }

    /// `clock` is the process's one wall clock; the Resources application stamps
    /// every Resource record and judges every File expiry with it.
    pub(crate) async fn open(
        backend: config::ResourceStoreBackend,
        postgres_schema: PostgresSchemaMode,
        clock: std::sync::Arc<dyn awaken_time::WallClock>,
        postgres_pools: &mut crate::postgres_pools::ProcessPostgresPools,
    ) -> Result<ProcessResources, String> {
        let invalidator = std::sync::Arc::new(awaken_sandbox_memoryd::LocalInvalidator::default());
        let application = match backend {
            config::ResourceStoreBackend::Embedded(root) => {
                awaken_resource_persistence::open_embedded(&root, clock, invalidator.clone())
                    .await
                    .map_err(|error| error.to_string())
            }
            config::ResourceStoreBackend::Postgres(url) => {
                awaken_resource_persistence::open_postgres_with_pool(
                    postgres_pools.get(&url).await?,
                    match postgres_schema {
                        PostgresSchemaMode::Migrate => {
                            awaken_resource_persistence::SchemaMode::Migrate
                        }
                        PostgresSchemaMode::Verify => {
                            awaken_resource_persistence::SchemaMode::Verify
                        }
                    },
                    None,
                    clock,
                    invalidator.clone(),
                )
                .await
                .map_err(|error| error.to_string())
            }
            config::ResourceStoreBackend::PostgresObject { url, object } => {
                awaken_resource_persistence::open_postgres_with_pool_and_object_store(
                    postgres_pools.get(&url).await?,
                    match postgres_schema {
                        PostgresSchemaMode::Migrate => {
                            awaken_resource_persistence::SchemaMode::Migrate
                        }
                        PostgresSchemaMode::Verify => {
                            awaken_resource_persistence::SchemaMode::Verify
                        }
                    },
                    object,
                    clock,
                    invalidator.clone(),
                )
                .await
                .map_err(|error| error.to_string())
            }
        }?;
        application
            .synchronize_skill_references()
            .await
            .map_err(|error| format!("restore Skill lifecycle references: {error}"))?;
        Ok(ProcessResources {
            application,
            invalidator,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{PostgresSchemaMode, ProcessResources, config};

    #[tokio::test]
    async fn every_local_backend_returns_one_complete_resources_application() {
        // FMECA: FM1 a backend selection returns only part of the Resources
        // authorities; FM2 process startup rebuilds services after persistence
        // selection and creates divergent identities. Cause/effect decision table:
        // R1 ephemeral selection -> one complete application and stable services;
        // R2 embedded selection -> the same contract over one durable root and
        // stable services. PostgreSQL Migrate/Verify are covered by adapter
        // integration suites; this test proves the process adapter never rebuilds
        // an application after persistence selection.
        let assert_complete = |resources: &ProcessResources| {
            let application = &resources.application;
            let authorities = application.authorities();
            let original_bus: Arc<dyn awaken_resource_contract::MemoryInvalidator> =
                resources.invalidator.clone();
            assert!(
                authorities
                    .memory_mount_source()
                    .uses_invalidator(&original_bus)
            );
            let _ = authorities.resource_registry();
            let _ = authorities.file_store();
            let _ = authorities.file_catalog();
            let _ = authorities.memory_queries();
            let _ = authorities.skill_store();
            let _ = authorities.reclamation();
            assert!(Arc::ptr_eq(
                &application.memory_stores(),
                &application.memory_stores()
            ));
            assert!(Arc::ptr_eq(
                &application.purge_scheduler(),
                &application.purge_scheduler()
            ));
        };
        assert_complete(&ProcessResources::ephemeral(awaken_time::system_clock()).await);

        let root = tempfile::tempdir().expect("resource application root");
        let embedded = ProcessResources::open(
            config::ResourceStoreBackend::Embedded(root.path().to_path_buf()),
            PostgresSchemaMode::Migrate,
            awaken_time::system_clock(),
            &mut crate::postgres_pools::ProcessPostgresPools::new(2),
        )
        .await
        .expect("embedded Resources application");
        assert_complete(&embedded);
    }
}
