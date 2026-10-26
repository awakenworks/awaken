// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: Static owner: the binding is an immutable projection of one frozen Session Memory
// @formal reason: input; every data-plane access re-validates the live resource before it reaches the
// @formal reason: canonical Memory content implementation, which keeps the only CAS rule. The one lock holds the
// @formal reason: physical Session commit an extraction writes through; absence makes extraction wait
// @formal reason: (Retryable) and never chooses another commit. Runtime tests drive both rows.
//! One Session-scoped MemoryStore binding, projected from the frozen Session
//! manifest, shared by Worker realization and Coordinator extraction.
//!
//! The binding retains the exact store, pinned config version,
//! granted access and live-resource validator, plus the data-plane operations
//! the Session's tools and recall use through it. The extraction behaviour that
//! runs an auxiliary agent over a binding stays in the host (`memory`), which
//! wraps a binding with its process-wide `MemoryRuntime`.

use crate::DeliveryLock;
use awaken_session_types::RunError;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use awaken_ext_memory::{MemoryExtractionIntent, MemoryStoreHandle};

use crate::{PlatformMemoryHandle, SessionCommit};

/// One Session-scoped MemoryStore binding shared by recall, the Session's
/// Memory tools and extraction.
pub struct MemoryBinding {
    /// Physical Session/commit partition that owns extraction execution. A
    /// delegated child's logical Thread remains in its transcript snapshot.
    pub session_thread_id: String,
    platform: Arc<PlatformMemoryHandle>,
    resource_validator: Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
    pub workspace_id: String,
    pub memory_store_id: String,
    pub memory_config_version: u64,
    pub recall_enabled: bool,
    pub extraction_enabled: bool,
    /// Attached once the physical Session's commit authority is known; an
    /// extraction waits until then. Shared by every holder of this binding.
    extraction_commit: RwLock<Option<Arc<SessionCommit>>>,
}

impl MemoryBinding {
    pub fn new(
        session_thread_id: impl Into<String>,
        workspace_id: impl Into<String>,
        platform: Arc<PlatformMemoryHandle>,
        resource_validator: Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
        config: &awaken_resource_contract::MemoryStoreConfigVersion,
        writable: bool,
    ) -> Self {
        Self {
            session_thread_id: session_thread_id.into(),
            platform,
            resource_validator,
            workspace_id: workspace_id.into(),
            memory_store_id: config.memory_store_id.to_string(),
            memory_config_version: config.version.0,
            // The binding grants data-plane availability only. Whether the main
            // Agent activates recall/extraction and with which bounds is owned by
            // its `memory` plugin configuration, never by a Store policy copy.
            recall_enabled: true,
            extraction_enabled: writable,
            extraction_commit: RwLock::new(None),
        }
    }

    /// Attach the Session's extraction commit. A poisoned context denies.
    pub fn attach_extraction_commit(&self, commit: Arc<SessionCommit>) -> Result<(), RunError> {
        *self
            .extraction_commit
            .write()
            .map_err(|_| DeliveryLock::MemoryExecutionContext)? = Some(commit);
        Ok(())
    }

    /// The attached extraction commit; a poisoned context denies the read
    /// instead of reporting that no commit is attached.
    pub fn extraction_commit(&self) -> Result<Option<Arc<SessionCommit>>, RunError> {
        Ok(self
            .extraction_commit
            .read()
            .map_err(|_| DeliveryLock::MemoryExecutionContext)?
            .clone())
    }

    pub fn matches_intent(&self, intent: &MemoryExtractionIntent) -> bool {
        intent.session_id == self.session_thread_id
            && intent.workspace_id == self.workspace_id
            && intent.memory_store_id == self.memory_store_id
            && intent.memory_config_version == self.memory_config_version
    }

    pub async fn validate_live_resource(&self) -> Result<(), String> {
        let Some(validator) = self.resource_validator.as_ref() else {
            return Ok(());
        };
        validator
            .verify_memory_binding(
                &self.workspace_id,
                &self.memory_store_id,
                awaken_resource_contract::ConfigVersion(self.memory_config_version),
            )
            .await
            .map_err(|error| error.to_string())
    }

    pub fn platform(&self) -> &PlatformMemoryHandle {
        &self.platform
    }
}

/// A live-validating Memory handle: every access re-checks the binding, so an
/// already-bound Session cannot bypass a later suspend/archive transition.
#[async_trait]
impl MemoryStoreHandle for MemoryBinding {
    async fn write(&self, name: &str, content: &str) -> Result<String, String> {
        self.validate_live_resource().await?;
        self.platform.write(name, content).await
    }

    async fn entries(&self) -> Result<Vec<awaken_ext_memory::Entry>, String> {
        self.validate_live_resource().await?;
        self.platform.entries().await
    }
}

#[cfg(test)]
mod tests {
    use super::{Arc, MemoryBinding, PlatformMemoryHandle, SessionCommit};

    #[test]
    fn memory_commit_binding_preserves_identity_and_refuses_poisoned_reads_and_writes() {
        // C1 no context / exact context attached; C2 the original context lock
        // is poisoned by a panicking holder. B1 !C1 => None, B2 C1 => the same
        // Arc, B3 C2 => both read and replacement refuse with the original
        // realization poison code and retryable class. No empty fallback or
        // replacement commit is permitted after the holder panics.
        let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
            awaken_time::system_clock(),
        ));
        let binding = MemoryBinding::new(
            "session",
            "workspace",
            Arc::new(PlatformMemoryHandle::from_roles(
                awaken_resource_contract::MemoryExecutionContent::new(repository),
                "memory".into(),
                true,
            )),
            None,
            &awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: "memory".into(),
                version: awaken_resource_contract::ConfigVersion(4),
                retention_policy: Default::default(),
            },
            true,
        );
        assert!(binding.extraction_commit().unwrap().is_none(), "B1");
        let commit: Arc<SessionCommit> = Arc::new(crate::SessionCommit::Local(Arc::new(
            awaken_dispatch_erasure::LocalCommitAdapter::projected(
                awaken_store_inmem::MemoryCommitCoordinator::new(awaken_time::system_clock()),
            ),
        )));
        binding.attach_extraction_commit(commit.clone()).unwrap();
        assert!(
            Arc::ptr_eq(&binding.extraction_commit().unwrap().unwrap(), &commit),
            "B2"
        );
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = binding.extraction_commit.write().unwrap();
            panic!("poison the original extraction commit lock");
        }));
        assert!(binding.extraction_commit.is_poisoned());
        let read_error = match binding.extraction_commit() {
            Err(error) => error,
            Ok(_) => panic!("B3 poisoned read cannot become missing context"),
        };
        let write_error = binding.attach_extraction_commit(commit).unwrap_err();
        for error in [read_error, write_error] {
            assert_eq!(
                error.kind,
                awaken_session_types::RunErrorKind::Unavailable,
                "B3"
            );
            assert_eq!(error.code, "session_realization_lock_poisoned", "B3");
            assert_eq!(
                error.message, "Memory execution context lock poisoned",
                "B3"
            );
        }
    }
}
