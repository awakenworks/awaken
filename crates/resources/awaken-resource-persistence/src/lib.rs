//! Resources-owned persistence selection.
//!
//! This is the only production factory that opens the File, Memory, Skill,
//! Resource Registry, and reclamation adapters as one application. Coordinator,
//! Runtime, and protocol crates consume the returned application and never
//! select or reopen a Resources database.

use std::path::Path;
use std::sync::Arc;

pub use awaken_file_store::object::{
    ObjectFileStoreConfig as ObjectBackingConfig, ObjectStoreProvider as ObjectBackingProvider,
};
use awaken_resource_application::{RegistryApplication, ResourceAuthorities, ResourcesApplication};
use awaken_resource_contract::MemoryInvalidator;
use awaken_time::WallClock;

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
    #[error("open Resource Registry: {0}")]
    Registry(String),
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
///
/// `clock` is the Resources context's one clock: the registry and every
/// Resources command stamp time with it (see [`ResourceAuthorities::clock`]).
pub async fn open_embedded(
    root: &Path,
    clock: Arc<dyn WallClock>,
    invalidator: Arc<dyn MemoryInvalidator>,
) -> Result<ResourcesApplication, ResourcePersistenceError> {
    std::fs::create_dir_all(root).map_err(|error| ResourcePersistenceError::CreateDirectory {
        path: root.to_path_buf(),
        error,
    })?;
    let resources = Arc::new(
        awaken_resource_store::SqliteResourceStore::open(root.join("resources.db"))
            .await
            .map_err(|error| ResourcePersistenceError::Registry(error.to_string()))?,
    );
    let registry = Arc::new(RegistryApplication::new(resources.clone(), clock.clone()));
    let memory_path = root.join("memory_fs.db");
    let memory_path = memory_path
        .to_str()
        .ok_or_else(|| ResourcePersistenceError::NonUtf8Path(memory_path.clone()))?;
    let memory = awaken_memory_store::SqliteMemoryRepository::open(memory_path, clock.clone())
        .await
        .map_err(|error| ResourcePersistenceError::Memory(error.to_string()))?;
    let file_path = root.join("files.db");
    let file_path = file_path
        .to_str()
        .ok_or_else(|| ResourcePersistenceError::NonUtf8Path(file_path.clone()))?;
    let files = Arc::new(
        awaken_file_store::sqlite::SqliteFileStore::open(file_path)
            .await
            .map_err(|error| ResourcePersistenceError::File(error.to_string()))?,
    );
    let skills = awaken_skill_store::FsSkillStore::open(root.join("skills"))
        .map_err(|error| ResourcePersistenceError::Skill(error.to_string()))?;
    Ok(ResourcesApplication::new(ResourceAuthorities::new(
        registry,
        files.clone(),
        files,
        Arc::new(
            awaken_resource_application::memory::InvalidatingMemoryRepository::new(
                Arc::new(memory),
                invalidator,
            ),
        ),
        Arc::new(skills),
        resources,
        clock,
    )))
}

/// Connect all Resources authorities to the same PostgreSQL installation.
pub async fn open_postgres(
    url: &str,
    schema: SchemaMode,
    clock: Arc<dyn WallClock>,
    invalidator: Arc<dyn MemoryInvalidator>,
) -> Result<ResourcesApplication, ResourcePersistenceError> {
    open_postgres_with_file_store(url, schema, None, clock, invalidator).await
}

/// Open PostgreSQL metadata authorities with an optional injected blob-byte
/// port. This remains one Resources application and one File domain authority.
pub async fn open_postgres_with_file_store(
    url: &str,
    schema: SchemaMode,
    blob_store: Option<Arc<dyn awaken_resource_contract::FileStore>>,
    clock: Arc<dyn WallClock>,
    invalidator: Arc<dyn MemoryInvalidator>,
) -> Result<ResourcesApplication, ResourcePersistenceError> {
    let pool = awaken_sql_storage::postgres_pool_options()
        .connect(url)
        .await
        .map_err(|error| ResourcePersistenceError::Registry(error.to_string()))?;
    open_postgres_with_pool(pool, schema, blob_store, clock, invalidator).await
}

/// Compose every PostgreSQL Resources adapter over one caller-owned pool.
/// The file-byte capability may remain remote; metadata keeps one database
/// authority and each existing adapter owns its scoped migration contract.
pub async fn open_postgres_with_pool(
    pool: sqlx::PgPool,
    schema: SchemaMode,
    blob_store: Option<Arc<dyn awaken_resource_contract::FileStore>>,
    clock: Arc<dyn WallClock>,
    invalidator: Arc<dyn MemoryInvalidator>,
) -> Result<ResourcesApplication, ResourcePersistenceError> {
    let resources = Arc::new(match schema {
        SchemaMode::Migrate => {
            let store = awaken_resource_store::PostgresResourceStore::with_pool(pool.clone());
            store
                .ensure_schema()
                .await
                .map_err(|error| ResourcePersistenceError::Registry(error.to_string()))?;
            store
        }
        SchemaMode::Verify => {
            awaken_resource_store::PostgresResourceStore::with_existing_pool(pool.clone())
                .await
                .map_err(|error| ResourcePersistenceError::Registry(error.to_string()))?
        }
    });
    let registry = Arc::new(RegistryApplication::new(resources.clone(), clock.clone()));
    let file_catalog = Arc::new(match schema {
        SchemaMode::Migrate => {
            let store = awaken_file_store::postgres::PgFileStore::with_pool(pool.clone());
            store
                .ensure_schema()
                .await
                .map_err(|error| ResourcePersistenceError::File(error.to_string()))?;
            store
        }
        SchemaMode::Verify => {
            awaken_file_store::postgres::PgFileStore::with_existing_pool(pool.clone())
                .await
                .map_err(|error| ResourcePersistenceError::File(error.to_string()))?
        }
    });
    let file_store = blob_store.unwrap_or_else(|| file_catalog.clone());
    let memory = match schema {
        SchemaMode::Migrate => {
            let store = awaken_memory_store::PostgresMemoryRepository::with_pool(
                pool.clone(),
                clock.clone(),
            );
            store
                .ensure_schema()
                .await
                .map_err(|error| ResourcePersistenceError::Memory(error.to_string()))?;
            store
        }
        SchemaMode::Verify => awaken_memory_store::PostgresMemoryRepository::with_existing_pool(
            pool.clone(),
            clock.clone(),
        )
        .await
        .map_err(|error| ResourcePersistenceError::Memory(error.to_string()))?,
    };
    let skills = match schema {
        SchemaMode::Migrate => {
            let store = awaken_skill_store::PgSkillStore::with_pool(pool.clone());
            store
                .ensure_schema()
                .await
                .map_err(|error| ResourcePersistenceError::Skill(error.to_string()))?;
            store
        }
        SchemaMode::Verify => awaken_skill_store::PgSkillStore::with_existing_pool(pool)
            .await
            .map_err(|error| ResourcePersistenceError::Skill(error.to_string()))?,
    };
    Ok(ResourcesApplication::new(ResourceAuthorities::new(
        registry,
        file_store,
        file_catalog,
        Arc::new(
            awaken_resource_application::memory::InvalidatingMemoryRepository::new(
                Arc::new(memory),
                invalidator,
            ),
        ),
        Arc::new(skills),
        resources,
        clock,
    )))
}

pub async fn open_postgres_with_object_store(
    url: &str,
    schema: SchemaMode,
    object: ObjectBackingConfig,
    clock: Arc<dyn WallClock>,
    invalidator: Arc<dyn MemoryInvalidator>,
) -> Result<ResourcesApplication, ResourcePersistenceError> {
    let pool = awaken_sql_storage::postgres_pool_options()
        .connect(url)
        .await
        .map_err(|error| ResourcePersistenceError::Registry(error.to_string()))?;
    open_postgres_with_pool_and_object_store(pool, schema, object, clock, invalidator).await
}

/// Inject the process metadata pool while selecting the existing object-byte adapter.
pub async fn open_postgres_with_pool_and_object_store(
    pool: sqlx::PgPool,
    schema: SchemaMode,
    object: ObjectBackingConfig,
    clock: Arc<dyn WallClock>,
    invalidator: Arc<dyn MemoryInvalidator>,
) -> Result<ResourcesApplication, ResourcePersistenceError> {
    let files = awaken_file_store::object::ObjectFileStore::from_config(object)
        .map_err(|error| ResourcePersistenceError::File(error.to_string()))?;
    open_postgres_with_pool(pool, schema, Some(Arc::new(files)), clock, invalidator).await
}

/// Hermetic Resources application for tests and scenario processes.
#[cfg(feature = "test-support")]
pub async fn ephemeral(
    clock: Arc<dyn WallClock>,
    invalidator: Arc<dyn MemoryInvalidator>,
) -> Result<ResourcesApplication, ResourcePersistenceError> {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let resources = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .map_err(|error| ResourcePersistenceError::Registry(error.to_string()))?,
    );
    let registry = Arc::new(RegistryApplication::new(resources.clone(), clock.clone()));
    Ok(ResourcesApplication::new(ResourceAuthorities::new(
        registry,
        files.clone(),
        files,
        Arc::new(
            awaken_resource_application::memory::InvalidatingMemoryRepository::new(
                Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                    clock.clone(),
                )),
                invalidator,
            ),
        ),
        Arc::new(awaken_skill_store::InMemorySkillStore::new()),
        resources,
        clock,
    )))
}

#[cfg(test)]
mod tests {
    #[derive(Default)]
    struct MemoryEvents(std::sync::Mutex<Vec<(String, String)>>);
    #[cfg(test)]
    impl awaken_resource_contract::MemoryInvalidator for MemoryEvents {
        fn publish(&self, store: &str, path: &str) {
            self.0.lock().unwrap().push((store.into(), path.into()));
        }
    }
    fn memory_events() -> Arc<dyn awaken_resource_contract::MemoryInvalidator> {
        Arc::new(MemoryEvents::default())
    }

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
            .memory_writer()
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
            .memory_queries()
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

    #[tokio::test]
    async fn embedded_selection_returns_one_complete_application() {
        // Cause/effect decision table:
        // R1 writable root -> one application exposing all six authorities and
        // stable MemoryStore/purge service identities; R2 root is a file -> typed
        // startup error and no partial application. PostgreSQL Migrate/Verify are
        // covered by each adapter's integration suite; this test owns atomic
        // embedded selection and the no-parallel-application invariant.
        let root = tempfile::tempdir().expect("R1 root");
        let app = open_embedded(root.path(), awaken_time::system_clock(), memory_events())
            .await
            .expect("R1 complete Resources application");
        let authorities = app.authorities();
        let _ = authorities.resource_registry();
        let _ = authorities.file_store();
        let _ = authorities.file_catalog();
        let _ = authorities.memory_queries();
        let _ = authorities.skill_store();
        let _ = authorities.reclamation();
        assert!(Arc::ptr_eq(&app.memory_stores(), &app.memory_stores()));
        assert!(Arc::ptr_eq(&app.purge_scheduler(), &app.purge_scheduler()));

        let invalid = tempfile::NamedTempFile::new().expect("R2 file");
        assert!(matches!(
            open_embedded(invalid.path(), awaken_time::system_clock(), memory_events()).await,
            Err(ResourcePersistenceError::CreateDirectory { .. })
        ));
    }

    #[tokio::test]
    async fn embedded_memory_content_uses_the_resource_authorities_clock() {
        // Cause/effect rule RC: one injected sub-millisecond process reading
        // must reach the Memory content head and version history unchanged,
        // while the Resource authorities retain that same clock handle.
        // A second ambient Memory clock would produce a different timestamp.
        const NANOS: u128 = 1_700_000_000_123_456_789;
        let clock: Arc<dyn WallClock> = Arc::new(awaken_lease_testkit::FixedClock::at(
            awaken_time::UtcTime::from_unix_nanos(NANOS),
        ));
        let root = tempfile::tempdir().unwrap();
        let app = open_embedded(root.path(), clock.clone(), memory_events())
            .await
            .unwrap();
        let authorities = app.authorities();
        assert!(Arc::ptr_eq(&authorities.clock(), &clock), "RC handle");
        let memory = authorities.memory_writer();
        let created = memory
            .create("clock-source", "/precise.md", "one")
            .await
            .unwrap();
        assert_eq!(created.created_unix_nanos, NANOS, "RC head");
        assert_eq!(created.updated_unix_nanos, NANOS, "RC head");
        assert_eq!(
            authorities
                .memory_versions()
                .list_versions("clock-source")
                .await
                .unwrap()[0]
                .created_unix_nanos,
            NANOS,
            "RC history"
        );
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
            let application = open_embedded(
                directory.path(),
                awaken_time::system_clock(),
                memory_events(),
            )
            .await?;
            seed_all_resource_authorities(&application, "embedded-restart").await?
        };
        let reopened = open_embedded(
            directory.path(),
            awaken_time::system_clock(),
            memory_events(),
        )
        .await?;
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
            let application = open_postgres(
                &url,
                SchemaMode::Migrate,
                awaken_time::system_clock(),
                memory_events(),
            )
            .await?;
            seed_all_resource_authorities(&application, &format!("postgres-{suffix}")).await?
        };
        let reopened = open_postgres(
            &url,
            SchemaMode::Verify,
            awaken_time::system_clock(),
            memory_events(),
        )
        .await?;
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
        let application = open_postgres_with_file_store(
            &url,
            SchemaMode::Migrate,
            Some(object.clone()),
            awaken_time::system_clock(),
            memory_events(),
        )
        .await
        .expect("open one hybrid Resources application");
        assert!(Arc::ptr_eq(
            &application.authorities().file_store(),
            &object
        ));
        let _ = application.authorities().file_catalog();
    }
}
