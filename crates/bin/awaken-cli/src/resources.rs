//! Process adapter for the canonical Resources persistence selection.

use super::{PostgresSchemaMode, config};

#[cfg(any(test, feature = "test-support"))]
pub(super) fn ephemeral_resources_application() -> awaken_resource_application::ResourcesApplication
{
    awaken_resource_persistence::ephemeral().expect("open ephemeral Resources application")
}

pub(super) async fn open_resources_application(
    backend: config::ResourceStoreBackend,
    postgres_schema: PostgresSchemaMode,
) -> Result<awaken_resource_application::ResourcesApplication, String> {
    let application = match backend {
        config::ResourceStoreBackend::Embedded(root) => {
            awaken_resource_persistence::open_embedded(&root).map_err(|error| error.to_string())
        }
        config::ResourceStoreBackend::Postgres(url) => awaken_resource_persistence::open_postgres(
            &url,
            match postgres_schema {
                PostgresSchemaMode::Migrate => awaken_resource_persistence::SchemaMode::Migrate,
                PostgresSchemaMode::Verify => awaken_resource_persistence::SchemaMode::Verify,
            },
        )
        .await
        .map_err(|error| error.to_string()),
        config::ResourceStoreBackend::PostgresObject { url, object } => {
            awaken_resource_persistence::open_postgres_with_object_store(
                &url,
                match postgres_schema {
                    PostgresSchemaMode::Migrate => awaken_resource_persistence::SchemaMode::Migrate,
                    PostgresSchemaMode::Verify => awaken_resource_persistence::SchemaMode::Verify,
                },
                object,
            )
            .await
            .map_err(|error| error.to_string())
        }
    }?;
    application
        .synchronize_skill_references()
        .await
        .map_err(|error| format!("restore Skill lifecycle references: {error}"))?;
    Ok(application)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

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
        let assert_complete = |application: &awaken_resource_application::ResourcesApplication| {
            let authorities = application.authorities();
            let _ = authorities.resource_registry();
            let _ = authorities.file_store();
            let _ = authorities.file_catalog();
            let _ = authorities.memory_repository();
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
        assert_complete(&ephemeral_resources_application());

        let root = tempfile::tempdir().expect("resource application root");
        let embedded = open_resources_application(
            config::ResourceStoreBackend::Embedded(root.path().to_path_buf()),
            PostgresSchemaMode::Migrate,
        )
        .await
        .expect("embedded Resources application");
        assert_complete(&embedded);
    }
}
