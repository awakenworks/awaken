//! One fault-only decorator over the original Resource Registry authority.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Fault-only decorator; the composed Resource Registry remains the sole state
/// and validation authority used by every test rule below.
pub(super) struct FaultInjectingResourceRegistry {
    inner: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    pub(super) registration_calls: AtomicUsize,
    pub(super) activation_calls: AtomicUsize,
    registration_failures: AtomicUsize,
    activation_failures: AtomicUsize,
    pub(super) memory_reads: Mutex<Vec<(String, String)>>,
    pub(super) memory_read_failure: Mutex<Option<awaken_resource_contract::ResourceRegistryError>>,
}

impl FaultInjectingResourceRegistry {
    pub(super) fn new(inner: Arc<dyn awaken_resource_contract::ResourceRegistry>) -> Self {
        Self {
            inner,
            registration_calls: AtomicUsize::new(0),
            activation_calls: AtomicUsize::new(0),
            registration_failures: AtomicUsize::new(0),
            activation_failures: AtomicUsize::new(0),
            memory_reads: Mutex::new(Vec::new()),
            memory_read_failure: Mutex::new(None),
        }
    }

    pub(super) fn fail_next_registration(&self) {
        self.registration_failures.store(1, Ordering::SeqCst);
    }

    pub(super) fn fail_next_activation(&self) {
        self.activation_failures.store(1, Ordering::SeqCst);
    }

    pub(super) fn consume_failure(counter: &AtomicUsize) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }
}

#[async_trait::async_trait]
impl awaken_resource_contract::ResourceInventory for FaultInjectingResourceRegistry {
    async fn list_repositories(
        &self,
        workspace_id: &str,
    ) -> Result<
        Vec<awaken_resource_contract::RepositoryDefinition>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.list_repositories(workspace_id).await
    }

    async fn find_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<
        Option<awaken_resource_contract::MemoryStoreDefinition>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.memory_reads
            .lock()
            .unwrap()
            .push((workspace_id.to_string(), id.to_string()));
        if let Some(error) = self.memory_read_failure.lock().unwrap().take() {
            return Err(error);
        }
        self.inner.find_memory_store(workspace_id, id).await
    }

    async fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<
        Vec<awaken_resource_contract::MemoryStoreDefinition>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.list_memory_stores(workspace_id).await
    }

    async fn find_memory_store_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<
        Option<awaken_resource_contract::MemoryStoreConfigVersion>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner
            .find_memory_store_config(workspace_id, id, version)
            .await
    }

    async fn find_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<
        Option<awaken_resource_contract::RepositoryDefinition>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.find_repository(workspace_id, id).await
    }

    async fn find_repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<
        Option<awaken_resource_contract::RepositoryConfigVersion>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner
            .find_repository_config(workspace_id, id, version)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_resource_contract::ResourceAdministration for FaultInjectingResourceRegistry {
    async fn register_repository_idempotent(
        &self,
        command: awaken_resource_contract::RegisterRepository,
    ) -> Result<
        awaken_resource_contract::RepositoryDefinition,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.register_repository_idempotent(command).await
    }

    async fn register_memory_store_idempotent(
        &self,
        command: awaken_resource_contract::RegisterMemoryStore,
    ) -> Result<
        awaken_resource_contract::MemoryStoreDefinition,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.register_memory_store_idempotent(command).await
    }

    async fn register_memory_store(
        &self,
        command: awaken_resource_contract::RegisterMemoryStore,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.register_memory_store(command).await
    }

    async fn update_memory_store_profile(
        &self,
        command: awaken_resource_contract::UpdateMemoryStoreProfile,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.update_memory_store_profile(command).await
    }

    async fn publish_memory_store_config(
        &self,
        command: awaken_resource_contract::PublishMemoryStoreConfig,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.publish_memory_store_config(command).await
    }

    async fn change_memory_store_state(
        &self,
        command: awaken_resource_contract::ChangeMemoryStoreState,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.change_memory_store_state(command).await
    }

    async fn register_repository(
        &self,
        command: awaken_resource_contract::RegisterRepository,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.registration_calls.fetch_add(1, Ordering::SeqCst);
        if Self::consume_failure(&self.registration_failures) {
            return Err(
                awaken_resource_contract::ResourceRegistryError::AuthorityUnavailable {
                    detail: "injected registration failure".into(),
                },
            );
        }
        self.inner.register_repository(command).await
    }

    async fn publish_repository_config(
        &self,
        command: awaken_resource_contract::PublishRepositoryConfig,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.publish_repository_config(command).await
    }

    async fn change_repository_state(
        &self,
        command: awaken_resource_contract::ChangeRepositoryState,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        if command.state == awaken_resource_contract::ResourceState::Active {
            self.activation_calls.fetch_add(1, Ordering::SeqCst);
            if Self::consume_failure(&self.activation_failures) {
                return Err(
                    awaken_resource_contract::ResourceRegistryError::AuthorityUnavailable {
                        detail: "injected activation failure".into(),
                    },
                );
            }
        }
        self.inner.change_repository_state(command).await
    }
}

#[async_trait::async_trait]
impl awaken_resource_contract::ExecutionResourceResolver for FaultInjectingResourceRegistry {
    async fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<
        awaken_resource_contract::MemoryStoreConfigVersion,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.resolve_memory_store(workspace_id, id).await
    }

    async fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<
        awaken_resource_contract::RepositoryConfigVersion,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.resolve_repository(workspace_id, id).await
    }
}

#[async_trait::async_trait]
impl awaken_resource_contract::LiveResourceBindingVerifier for FaultInjectingResourceRegistry {
    async fn verify_memory_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner
            .verify_memory_binding(workspace_id, id, version)
            .await
    }

    async fn verify_repository_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner
            .verify_repository_binding(workspace_id, id, version)
            .await
    }
}
