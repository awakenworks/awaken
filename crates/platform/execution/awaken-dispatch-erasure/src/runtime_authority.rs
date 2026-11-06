// @formal surface obligation=remote_worker.committed_truth_is_coordinator_owned
// @formal reason: The original selected durable capability and typed errors are declarations only.
// @formal reason: Concrete pools/stores/Clock/initialization remain Coordinator-owned; Host local/remote
// @formal reason: permission and commit decisions stay in SessionAuthority. This module opens no backend.
//! The existing durable capability selected by process composition.

use crate::{AnyCoordinatorDispatch, LocalCommit};
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use std::sync::Arc;

/// The complete durable capability injected into one Coordinator Runtime Host.
///
/// Source-owner rule A3: composition consumes this original canonical declaration;
/// it does not select a backend or manufacture an implementation here. Existing
/// Coordinator/Host tables retain the runtime authorization and failure laws.
/// ```
/// use std::sync::Arc;
/// use awaken_dispatch_erasure::{RuntimeAuthority, RuntimeAuthorityError};
/// fn selected(authority: Arc<dyn RuntimeAuthority>) -> Arc<dyn RuntimeAuthority> { authority }
/// let _: RuntimeAuthorityError = RuntimeAuthorityError::unavailable("source input");
/// ```
#[async_trait::async_trait]
pub trait RuntimeAuthority: Send + Sync {
    async fn open_commit(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn LocalCommit>, RuntimeAuthorityError>;

    async fn durable_thread_exists(&self, thread: &str) -> Result<bool, RuntimeAuthorityError>;

    fn dispatch_store(&self) -> Arc<AnyCoordinatorDispatch>;

    fn dispatch_wake(&self) -> Option<Arc<dyn awaken_dispatch_contract::WakeSignal>>;

    fn stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn StreamCheckpointStore>, RuntimeAuthorityError>;
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeAuthorityError {
    #[error("runtime authority unavailable: {0}")]
    Unavailable(String),
    #[error("runtime authority is corrupt: {0}")]
    Corrupt(String),
    #[error("runtime authority is misconfigured: {0}")]
    Misconfigured(String),
}

impl RuntimeAuthorityError {
    pub fn unavailable(error: impl Into<String>) -> Self {
        Self::Unavailable(error.into())
    }

    pub fn misconfigured(error: impl Into<String>) -> Self {
        Self::Misconfigured(error.into())
    }
}
