//! Local and ephemeral stores for product-process startup.

use super::*;

/// Ephemeral deployment stores: everything in process memory (dev / e2e default).
#[cfg(any(test, feature = "test-support"))]
pub(super) fn in_memory_process_stores() -> ProcessStores {
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let sessions = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("open ephemeral managed Session repository"),
    );
    let admin = Arc::new(
        awaken_admin_config_api::SqliteAdminStore::open_in_memory()
            .expect("open ephemeral admin store"),
    );
    let data_subjects = Arc::new(awaken_data_subject_store::InMemoryDataSubjectRepo::new());
    let captured_content =
        Arc::new(awaken_captured_content_store::InMemoryCapturedContentStore::new());
    ProcessStores {
        workspace_root: None,
        control: Some(ControlStores {
            catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
            credentials: credentials.clone(),
            vaults: credentials,
            secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
            profiles: admin.clone(),
            resources: admin.clone(),
            webhooks: admin,
            config: Arc::new(
                awaken_config_store::SqliteConfigStore::open_in_memory()
                    .expect("open config store"),
            ),
            data_subjects: data_subjects.clone(),
            erasure_jobs: data_subjects,
            environments: Arc::new(awaken_env_store::InMemoryEnvRegistry::new()),
            sandbox_policies: Arc::new(
                awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default(),
            ),
        }),
        coordinator: Some(CoordinatorStores {
            resources: ephemeral_resources_application(),
            sessions: sessions.clone(),
            deployments: sessions.clone(),
            memory_extractions: sessions.clone(),
            dream_process_store: sessions,
            capture_sink: captured_content.clone(),
            captured_content_eraser: captured_content,
            environment_work: Arc::new(awaken_work_store::InMemoryWorkQueue::new()),
        }),
    }
}

#[cfg(test)]
pub(super) fn in_memory_control_stores() -> ProcessStores {
    let mut stores = in_memory_process_stores();
    stores.coordinator = None;
    stores
}

#[cfg(test)]
pub(super) fn in_memory_split_coordinator() -> (ProcessStores, ControlServices) {
    let mut stores = in_memory_process_stores();
    let control = stores
        .control
        .take()
        .expect("fixture starts with Control stores");
    let audit = Arc::new(ManagementAuditPlane::new(control.config.clone()));
    let credentials = Arc::new(
        awaken_protocol_managed::VaultState::new(
            control.secrets.clone(),
            control.credentials.clone(),
            control.vaults.clone(),
        )
        .with_probe(Arc::new(ExtMcpProbe)),
    );
    let webhooks = awaken_webhook_managed::config_plane_lifecycle_delivery(
        control.webhooks,
        control.secrets,
        None,
    );
    let consent = Arc::new(
        awaken_data_subject_application::RepoDataSubjectResolver::new(
            control.data_subjects,
            control.erasure_jobs,
        ),
    );
    (
        stores,
        ControlServices {
            audit,
            credentials,
            webhooks,
            consent,
        },
    )
}

/// Keep the Managed Session aggregate durable whenever the runtime itself is
/// durable, even when the rest of the process startup intentionally remains
/// ephemeral. A restarted runtime can only rehydrate a governed Session when its
/// configuration and owner fence survive beside the committed thread facts.
#[cfg(test)]
pub(super) fn process_stores_for_runtime_storage(
    storage_dir: Option<&std::path::Path>,
) -> ProcessStores {
    let mut stores = in_memory_process_stores();
    let Some(dir) = storage_dir else {
        return stores;
    };
    stores.workspace_root = Some(dir.to_path_buf());
    std::fs::create_dir_all(dir).expect("create runtime storage directory");
    let sessions = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(
            &dir.join("sessions.db").to_string_lossy(),
        )
        .expect("open sessions.db under runtime storage directory"),
    );
    let coordinator = stores
        .coordinator
        .as_mut()
        .expect("test startup owns Coordinator");
    coordinator.sessions = sessions.clone();
    coordinator.deployments = sessions.clone();
    coordinator.memory_extractions = sessions.clone();
    coordinator.dream_process_store = sessions;
    stores
}

/// Local SQLite is a backend selection, not a second store process.
pub(super) async fn open_local_process_stores(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> Result<ProcessStores, String> {
    let resources = open_resources_application(
        config::ResourceStoreBackend::Embedded(dir.to_path_buf()),
        PostgresSchemaMode::Migrate,
    )
    .await?;
    open_process_stores(ProcessStoreOpenOptions {
        control: awaken_control::ControlStoreConfig::local(dir),
        coordinator: config::CoordinatorStoreConfig {
            sessions: awaken_control::StoreBackend::Sqlite(dir.join("sessions.db")),
            captured_content: awaken_control::StoreBackend::Sqlite(dir.join("captured_content.db")),
        },
        resources: Some(resources),
        workspace_root: dir.to_path_buf(),
        seal_key: Some(key),
        role: config::Role::AllInOne,
        postgres_schema: PostgresSchemaMode::Migrate,
    })
    .await
}
