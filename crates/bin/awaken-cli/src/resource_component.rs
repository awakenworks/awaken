//! Process adapters for the canonical Resources component.

use std::sync::Arc;

use super::{PostgresSchemaMode, config};

pub(super) fn ephemeral_resource_component() -> awaken_resource_contract::ResourceComponent {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    awaken_resource_contract::build_resource_component(
        awaken_resource_contract::ResourceDependencies {
            file_store: files.clone(),
            file_catalog: files,
            memory_repository: Arc::new(awaken_memory_store::VolatileMemoryRepository::new()),
            skill_store: Arc::new(awaken_skill_store::InMemorySkillStore::new()),
            lifecycle: Arc::new(
                awaken_resource_store::SqliteResourceStore::in_memory()
                    .expect("open ephemeral resource lifecycle sqlite"),
            ),
        },
    )
}

pub(super) async fn open_resource_component(
    backend: config::ResourcePlaneStoreBackend,
    postgres_schema: PostgresSchemaMode,
) -> Result<awaken_resource_contract::ResourceComponent, String> {
    match backend {
        config::ResourcePlaneStoreBackend::Embedded(root) => {
            Ok(awaken_server::embedded_resource_component(&root))
        }
        config::ResourcePlaneStoreBackend::Postgres(url) => {
            let lifecycle = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_resource_store::PostgresResourceStore::connect(&url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_resource_store::PostgresResourceStore::connect_existing(&url).await
                }
            }
            .map_err(|error| format!("connect resource lifecycle Postgres: {error}"))?;
            let files = Arc::new(
                match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_file_store::postgres::PgFileStore::connect(&url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_file_store::postgres::PgFileStore::connect_existing(&url).await
                    }
                }
                .map_err(|error| format!("connect resource file Postgres: {error}"))?,
            );
            let memory = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_memory_store::PostgresMemoryRepository::connect(&url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_memory_store::PostgresMemoryRepository::connect_existing(&url).await
                }
            }
            .map_err(|error| format!("connect resource memory Postgres: {error}"))?;
            let skills = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_skill_store::PgSkillStore::connect(&url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_skill_store::PgSkillStore::connect_existing(&url).await
                }
            }
            .map_err(|error| format!("connect resource skill Postgres: {error}"))?;
            Ok(awaken_resource_contract::build_resource_component(
                awaken_resource_contract::ResourceDependencies {
                    file_store: files.clone(),
                    file_catalog: files,
                    memory_repository: Arc::new(memory),
                    skill_store: Arc::new(skills),
                    lifecycle: Arc::new(lifecycle),
                },
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_local_backend_returns_one_complete_resource_component() {
        // Cause/effect decision table:
        // R1 ephemeral selection -> all five per-kind/lifecycle ports exist;
        // R2 embedded selection -> all five ports exist over one durable root.
        // PostgreSQL adapter selection is covered by backend integration suites;
        // this unit test owns the no-parallel-construction component invariant.
        let assert_complete = |component: &awaken_resource_contract::ResourceComponent| {
            let _ = component.file_store();
            let _ = component.file_catalog();
            let _ = component.memory_repository();
            let _ = component.skill_store();
            let _ = component.lifecycle();
        };
        assert_complete(&ephemeral_resource_component());

        let root = tempfile::tempdir().expect("resource component root");
        let embedded = open_resource_component(
            config::ResourcePlaneStoreBackend::Embedded(root.path().to_path_buf()),
            PostgresSchemaMode::Migrate,
        )
        .await
        .expect("embedded Resources component");
        assert_complete(&embedded);
    }
}
