// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: Static owner: the binding is an immutable projection of one frozen Session Memory
// @formal reason: input; every data-plane access re-validates the live resource before it reaches the
// @formal reason: canonical Memory content implementation, which keeps the only CAS rule. The one lock holds the
// @formal reason: physical Session commit an extraction writes through; absence makes extraction wait
// @formal reason: (Retryable) and never chooses another commit. Runtime tests drive both rows.
//! One Session-scoped MemoryStore binding, projected from the frozen Session
//! manifest during realization.
//!
//! The binding is realization's value: the exact store, pinned config version,
//! granted access and live-resource validator, plus the data-plane operations
//! the Session's tools and recall use through it. The extraction behaviour that
//! runs an auxiliary agent over a binding stays in the host (`memory`), which
//! wraps a binding with its process-wide `MemoryRuntime`.

use crate::realization_error::{RealizationError, RealizationLock};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use awaken_agent_contract::CommitCoordinator;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_ext_memory::{MemoryExtractionIntent, MemoryStoreHandle};

mod platform;
pub use platform::PlatformMemoryHandle;

/// The physical Session commit authority an extraction over this binding
/// writes through and reads its committed transcript back from.
pub trait MemoryExtractionCommit: CommitCoordinator + CommittedThreadView {}

impl<T: CommitCoordinator + CommittedThreadView + ?Sized> MemoryExtractionCommit for T {}

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
    extraction_commit: RwLock<Option<Arc<dyn MemoryExtractionCommit>>>,
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
    pub fn attach_extraction_commit(
        &self,
        commit: Arc<dyn MemoryExtractionCommit>,
    ) -> Result<(), RealizationError> {
        *self
            .extraction_commit
            .write()
            .map_err(|_| RealizationLock::MemoryExecutionContext)? = Some(commit);
        Ok(())
    }

    /// The attached extraction commit; a poisoned context denies the read
    /// instead of reporting that no commit is attached.
    pub fn extraction_commit(
        &self,
    ) -> Result<Option<Arc<dyn MemoryExtractionCommit>>, RealizationError> {
        Ok(self
            .extraction_commit
            .read()
            .map_err(|_| RealizationLock::MemoryExecutionContext)?
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

/// One injected transport adapter encodes both ordinary Run-scoped and
/// terminal Session-scoped Memory operations. The generic Resource ports stay
/// authoritative; this private intersection trait only permits one trait
/// object to carry both implementations through Runtime composition.
pub trait SessionMemoryReferenceEncoder:
    awaken_resource_contract::MemoryMaterializationReferenceEncoder<
        awaken_dispatch_contract::RunClaim,
    > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
        awaken_session_realization_contract::SessionResourceMaterializationIntent,
    > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
        awaken_session_state_contract::SessionTerminalMemoryIntent,
    >
{
}

impl<T> SessionMemoryReferenceEncoder for T where
    T: awaken_resource_contract::MemoryMaterializationReferenceEncoder<
            awaken_dispatch_contract::RunClaim,
        > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
            awaken_session_realization_contract::SessionResourceMaterializationIntent,
        > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
            awaken_session_state_contract::SessionTerminalMemoryIntent,
        >
{
}
