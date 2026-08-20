//! Resources-owned persistence selection.
//!
//! This is the only production factory that opens the File, Memory, Skill,
//! Resource Catalog, and reclamation adapters as one application. Coordinator,
//! Runtime, and protocol crates consume the returned application and never
//! select or reopen a Resources database.

use std::path::Path;
use std::sync::Arc;

pub use awaken_file_store::object::{
    ObjectFileStoreConfig as ObjectBackingConfig, ObjectStoreProvider as ObjectBackingProvider,
};
use awaken_resource_application::{ResourceAuthorities, ResourcesApplication};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaMode {
    Migrate,
    Verify,
}

#[derive(Debug, thiserror::Error)]
pub enum ResourcePersistenceError {
    #[error("create Resources directory {path}: {error}")]
    CreateDirectory {
        path: std::path::PathBuf,
        error: std::io::Error,
    },
    #[error("open Resource Catalog: {0}")]
    Catalog(String),
    #[error("open File store: {0}")]
    File(String),
    #[error("open Memory store: {0}")]
    Memory(String),
    #[error("open Skill store: {0}")]
    Skill(String),
    #[error("Resources path is not valid UTF-8: {0}")]
    NonUtf8Path(std::path::PathBuf),
}

/// Open one embedded Resources authority rooted beneath `root`.
pub fn open_embedded(root: &Path) -> Result<ResourcesApplication, ResourcePersistenceError> {
    std::fs::create_dir_all(root).map_err(|error| ResourcePersistenceError::CreateDirectory {
        path: root.to_path_buf(),
        error,
    })?;
    let resources = Arc::new(
        awaken_resource_store::SqliteResourceStore::open(root.join("resources.db"))
            .map_err(|error| ResourcePersistenceError::Catalog(error.to_string()))?,
    );
    let memory_path = root.join("memory_fs.db");
    let memory_path = memory_path
        .to_str()
        .ok_or_else(|| ResourcePersistenceError::NonUtf8Path(memory_path.clone()))?;
    let memory = awaken_memory_store::SqliteMemoryRepository::open(memory_path)
        .map_err(|error| ResourcePersistenceError::Memory(error.to_string()))?;
    let file_path = root.join("files.db");
    let file_path = file_path
        .to_str()
        .ok_or_else(|| ResourcePersistenceError::NonUtf8Path(file_path.clone()))?;
    let files = Arc::new(
        awaken_file_store::sqlite::SqliteFileStore::open(file_path)
            .map_err(|error| ResourcePersistenceError::File(error.to_string()))?,
    );
    let skills = awaken_skill_store::FsSkillStore::open(root.join("skills"))
        .map_err(|error| ResourcePersistenceError::Skill(error.to_string()))?;
    Ok(ResourcesApplication::new(ResourceAuthorities::new(
        resources.clone(),
        files.clone(),
        files,
        Arc::new(memory),
        Arc::new(skills),
        resources,
    )))
}

/// Connect all Resources authorities to the same PostgreSQL installation.
pub async fn open_postgres(
    url: &str,
    schema: SchemaMode,
) -> Result<ResourcesApplication, ResourcePersistenceError> {
    open_postgres_with_file_store(url, schema, None).await
}

/// Open PostgreSQL metadata authorities with an optional injected blob-byte
/// port. This remains one Resources application and one File domain authority.
pub async fn open_postgres_with_file_store(
    url: &str,
    schema: SchemaMode,
    blob_store: Option<Arc<dyn awaken_resource_contract::FileStore>>,
) -> Result<ResourcesApplication, ResourcePersistenceError> {
    let resources = Arc::new(
        match schema {
            SchemaMode::Migrate => awaken_resource_store::PostgresResourceStore::connect(url).await,
            SchemaMode::Verify => {
                awaken_resource_store::PostgresResourceStore::connect_existing(url).await
            }
        }
        .map_err(|error| ResourcePersistenceError::Catalog(error.to_string()))?,
    );
    let file_catalog = Arc::new(
        match schema {
            SchemaMode::Migrate => awaken_file_store::postgres::PgFileStore::connect(url).await,
            SchemaMode::Verify => {
                awaken_file_store::postgres::PgFileStore::connect_existing(url).await
            }
        }
        .map_err(|error| ResourcePersistenceError::File(error.to_string()))?,
    );
    let file_store = blob_store.unwrap_or_else(|| file_catalog.clone());
    let memory = match schema {
        SchemaMode::Migrate => awaken_memory_store::PostgresMemoryRepository::connect(url).await,
        SchemaMode::Verify => {
            awaken_memory_store::PostgresMemoryRepository::connect_existing(url).await
        }
    }
    .map_err(|error| ResourcePersistenceError::Memory(error.to_string()))?;
    let skills = match schema {
        SchemaMode::Migrate => awaken_skill_store::PgSkillStore::connect(url).await,
        SchemaMode::Verify => awaken_skill_store::PgSkillStore::connect_existing(url).await,
    }
    .map_err(|error| ResourcePersistenceError::Skill(error.to_string()))?;
    Ok(ResourcesApplication::new(ResourceAuthorities::new(
        resources.clone(),
        file_store,
        file_catalog,
        Arc::new(memory),
        Arc::new(skills),
        resources,
    )))
}

pub async fn open_postgres_with_object_store(
    url: &str,
    schema: SchemaMode,
    object: ObjectBackingConfig,
) -> Result<ResourcesApplication, ResourcePersistenceError> {
    let files = awaken_file_store::object::ObjectFileStore::from_config(object)
        .map_err(|error| ResourcePersistenceError::File(error.to_string()))?;
    open_postgres_with_file_store(url, schema, Some(Arc::new(files))).await
}

/// Hermetic Resources application for tests and scenario processes.
#[cfg(feature = "test-support")]
pub fn ephemeral() -> Result<ResourcesApplication, ResourcePersistenceError> {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let resources = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .map_err(|error| ResourcePersistenceError::Catalog(error.to_string()))?,
    );
    Ok(ResourcesApplication::new(ResourceAuthorities::new(
        resources.clone(),
        files.clone(),
        files,
        Arc::new(awaken_memory_store::VolatileMemoryRepository::new()),
        Arc::new(awaken_skill_store::InMemorySkillStore::new()),
        resources,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::{
        CreateMemoryStoreCommand, ResourceState, SkillBundleFile, SkillDefinition, SkillVersion,
    };
    use awaken_skill_store::bundle_sha256;

    struct PersistedFixture {
        workspace_id: String,
        memory_store_id: String,
        file_id: String,
        blob_id: String,
        skill_id: String,
    }

    async fn seed_all_resource_authorities(
        application: &ResourcesApplication,
        suffix: &str,
    ) -> Result<PersistedFixture, Box<dyn std::error::Error>> {
        let workspace_id = format!("workspace-{suffix}");
        let memory_store_id = format!("memory-{suffix}");
        application
            .memory_stores()
            .create(CreateMemoryStoreCommand {
                workspace_id: workspace_id.clone(),
                id: Some(memory_store_id.as_str().into()),
                name: "durable memory".into(),
                description: "restart fixture".into(),
                metadata: Default::default(),
                initial_state: ResourceState::Active,
                retention_policy: Default::default(),
            })
            .await?;
        application
            .authorities()
            .memory_repository()
            .create(
                &memory_store_id,
                "/notes/evidence.txt",
                "durable memory bytes",
            )
            .await?;

        let file = application
            .files()
            .create_uploaded_file(
                &workspace_id,
                "evidence.txt".into(),
                "text/plain".into(),
                b"durable file bytes",
            )
            .await?;
        let file_id = file.id;
        let blob_id = file.blob_id;

        let skill_id = format!("skill-{suffix}");
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\ndescription: restart fixture\n---\nDurable skill".to_vec(),
            executable: false,
        }];
        application
            .authorities()
            .skill_store()
            .create(
                SkillDefinition {
                    id: skill_id.as_str().into(),
                    workspace_id: workspace_id.clone(),
                    display_title: Some("durable skill".into()),
                    latest_version: 1,
                    last_version: 1,
                    timestamps: Default::default(),
                },
                SkillVersion {
                    id: format!("skill-version-{suffix}").into(),
                    skill_id: skill_id.as_str().into(),
                    version: 1,
                    name: "restart-fixture".into(),
                    description: "restart fixture".into(),
                    directory: "/skills/restart-fixture".into(),
                    bundle_sha256: bundle_sha256(&files),
                    files,
                    created_unix_nanos: 1,
                },
            )
            .await?;

        Ok(PersistedFixture {
            workspace_id,
            memory_store_id,
            file_id,
            blob_id,
            skill_id,
        })
    }

    async fn assert_all_resource_authorities(
        application: &ResourcesApplication,
        fixture: &PersistedFixture,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let authorities = application.authorities();
        assert!(
            application
                .memory_stores()
                .get(&fixture.workspace_id, &fixture.memory_store_id)
                .await?
                .is_some()
        );
        let memory = authorities
            .memory_repository()
            .get_by_path(&fixture.memory_store_id, "/notes/evidence.txt")
            .await?
            .ok_or_else(|| std::io::Error::other("Memory content missing after restart"))?;
        assert_eq!(memory.content.as_deref(), Some("durable memory bytes"));

        let file = application
            .files()
            .get(&fixture.workspace_id, &fixture.file_id)
            .await?
            .ok_or_else(|| std::io::Error::other("File metadata missing after restart"))?;
        assert_eq!(file.blob_id, fixture.blob_id);
        assert_eq!(
            authorities
                .file_store()
                .get(&fixture.blob_id)
                .await?
                .ok_or_else(|| std::io::Error::other("File bytes missing after restart"))?,
            b"durable file bytes"
        );

        let skill = authorities
            .skill_store()
            .definition(&fixture.workspace_id, &fixture.skill_id)
            .await?
            .ok_or_else(|| std::io::Error::other("Skill definition missing after restart"))?;
        assert_eq!(skill.latest_version, 1);
        let version = authorities
            .skill_store()
            .version(&fixture.workspace_id, &fixture.skill_id, 1)
            .await?
            .ok_or_else(|| std::io::Error::other("Skill version missing after restart"))?;
        assert_eq!(
            version.skill_md(),
            Some(b"---\ndescription: restart fixture\n---\nDurable skill".as_slice())
        );
        Ok(())
    }

    #[test]
    fn embedded_selection_returns_one_complete_application() {
        // Cause/effect decision table:
        // R1 writable root -> one application exposing all six authorities and
        // stable MemoryStore/purge service identities; R2 root is a file -> typed
        // startup error and no partial application. PostgreSQL Migrate/Verify are
        // covered by each adapter's integration suite; this test owns atomic
        // embedded selection and the no-parallel-application invariant.
        let root = tempfile::tempdir().expect("R1 root");
        let app = open_embedded(root.path()).expect("R1 complete Resources application");
        let authorities = app.authorities();
        let _ = authorities.resource_catalog();
        let _ = authorities.file_store();
        let _ = authorities.file_catalog();
        let _ = authorities.memory_repository();
        let _ = authorities.skill_store();
        let _ = authorities.reclamation();
        assert!(Arc::ptr_eq(&app.memory_stores(), &app.memory_stores()));
        assert!(Arc::ptr_eq(&app.purge_scheduler(), &app.purge_scheduler()));

        let invalid = tempfile::NamedTempFile::new().expect("R2 file");
        assert!(matches!(
            open_embedded(invalid.path()),
            Err(ResourcePersistenceError::CreateDirectory { .. })
        ));
    }

    #[tokio::test]
    async fn embedded_resource_authorities_survive_application_restart()
    -> Result<(), Box<dyn std::error::Error>> {
        // State-transition decision table shared with the PostgreSQL test:
        // C1=one application persists Catalog identity, Memory content, File
        // metadata+bytes, and a versioned Skill; C2=all handles are dropped;
        // C3=the same backend is reopened. R1(C1+C2+C3)->every authority returns
        // the exact durable value; R2 prevents an in-memory/file-system fallback
        // from making only part of the Resources aggregate appear recovered.
        let directory = tempfile::tempdir()?;
        let fixture = {
            let application = open_embedded(directory.path())?;
            seed_all_resource_authorities(&application, "embedded-restart").await?
        };
        let reopened = open_embedded(directory.path())?;
        assert_all_resource_authorities(&reopened, &fixture).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_resource_authorities_survive_verify_mode_restart()
    -> Result<(), Box<dyn std::error::Error>> {
        // Same decision table as embedded. The second open uses Verify, adding
        // the managed-deployment rule that an application restart performs no
        // opportunistic DDL and still recovers every resource authority.
        let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
            return Ok(());
        };
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let fixture = {
            let application = open_postgres(&url, SchemaMode::Migrate).await?;
            seed_all_resource_authorities(&application, &format!("postgres-{suffix}")).await?
        };
        let reopened = open_postgres(&url, SchemaMode::Verify).await?;
        assert_all_resource_authorities(&reopened, &fixture).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_metadata_and_injected_blob_store_form_one_application() {
        // Cause/effect graph: C1=PostgreSQL authorities open, C2=one injected
        // FileStore opens, C3=application construction is atomic. Decision
        // table: C1+C2+C3 -> metadata remains PostgreSQL while blob bytes use
        // the injected port; !C1|!C2 -> startup error/no partial application.
        // The live row runs only with the canonical integration database.
        let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
            return;
        };
        let object: Arc<dyn awaken_resource_contract::FileStore> =
            Arc::new(awaken_file_store::object::ObjectFileStore::new(
                Arc::new(object_store::memory::InMemory::new()),
                "deployment/files",
            ));
        let application =
            open_postgres_with_file_store(&url, SchemaMode::Migrate, Some(object.clone()))
                .await
                .expect("open one hybrid Resources application");
        assert!(Arc::ptr_eq(
            &application.authorities().file_store(),
            &object
        ));
        let _ = application.authorities().file_catalog();
    }
}
