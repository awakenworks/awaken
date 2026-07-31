//! Resource-plane backend assembly for the product composition root.

use std::sync::Arc;

use super::{PostgresSchemaMode, config};

pub(super) fn ephemeral_resource_plane() -> awaken_runtime_host::ResourcePlane {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    awaken_runtime_host::ResourcePlane::new(
        files.clone(),
        files,
        Arc::new(awaken_memory_store::VolatileMemoryRepository::new()),
        Arc::new(awaken_skill_store::InMemorySkillStore::new()),
        Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .expect("open ephemeral resource lifecycle sqlite"),
        ),
    )
}

pub(super) async fn open_resource_plane(
    backend: config::ResourcePlaneStoreBackend,
    postgres_schema: PostgresSchemaMode,
) -> Result<awaken_runtime_host::ResourcePlane, String> {
    match backend {
        config::ResourcePlaneStoreBackend::Embedded(root) => {
            Ok(awaken_server::embedded_resource_plane(&root))
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
            Ok(awaken_runtime_host::ResourcePlane::new(
                files.clone(),
                files,
                Arc::new(memory),
                Arc::new(skills),
                Arc::new(lifecycle),
            ))
        }
    }
}
