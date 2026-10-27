// @formal surface obligation=remote_worker.committed_truth_is_coordinator_owned
// @formal reason: Test-support reference implementation moved intact from the SDK; production roots
// @formal reason: still inject durable authority, and no queue/backend is selected by this adapter.
//! The one volatile reference Runtime authority, enabled only for test support.

use crate::{LocalCommit, LocalCommitAdapter, RuntimeAuthority, RuntimeAuthorityError};
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use std::sync::Arc;

/// Process-local authority used only by unit tests and scenario fixtures.
///
/// This is the sole non-durable reference adapter for the runtime boundary. It
/// keeps product construction fail-closed while allowing tests to exercise the
/// same injected contract without opening SQL or filesystem stores here.
pub struct ReferenceAuthority {
    /// One clock for the dispatch store and every Thread's commit store.
    clock: Arc<dyn awaken_time::WallClock>,
    commits: std::sync::Mutex<std::collections::HashMap<String, Arc<dyn LocalCommit>>>,
    checkpoints:
        std::sync::Mutex<std::collections::HashMap<String, Arc<dyn StreamCheckpointStore>>>,
    dispatch: Arc<crate::AnyCoordinatorDispatch>,
}

impl ReferenceAuthority {
    /// Seed the existing test authority with a deliberately stale query adapter.
    pub fn with_commit(self, thread: &str, commit: Arc<dyn LocalCommit>) -> Self {
        self.commits
            .lock()
            .unwrap()
            .insert(thread.to_owned(), commit);
        self
    }

    /// The scenario composition supplies the exact backend-erased Dispatch
    /// authority, including its continuation role; this shared Host selects no
    /// concrete Coordinator queue implementation.
    #[must_use]
    pub fn with_dispatch(
        clock: Arc<dyn awaken_time::WallClock>,
        dispatch: Arc<crate::AnyCoordinatorDispatch>,
    ) -> Self {
        Self {
            clock,
            commits: std::sync::Mutex::new(std::collections::HashMap::new()),
            checkpoints: std::sync::Mutex::new(std::collections::HashMap::new()),
            dispatch,
        }
    }
}

#[async_trait::async_trait]
impl RuntimeAuthority for ReferenceAuthority {
    async fn open_commit(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn LocalCommit>, RuntimeAuthorityError> {
        let mut commits = self.commits.lock().map_err(|_| {
            RuntimeAuthorityError::Corrupt("ephemeral commit authority lock poisoned".to_owned())
        })?;
        Ok(commits
            .entry(thread.to_owned())
            .or_insert_with(|| {
                Arc::new(LocalCommitAdapter::projected(
                    awaken_store_inmem::MemoryCommitCoordinator::new(self.clock.clone()),
                )) as Arc<dyn LocalCommit>
            })
            .clone())
    }

    async fn durable_thread_exists(&self, thread: &str) -> Result<bool, RuntimeAuthorityError> {
        self.commits
            .lock()
            .map(|commits| commits.contains_key(thread))
            .map_err(|_| {
                RuntimeAuthorityError::Corrupt(
                    "ephemeral commit authority lock poisoned".to_owned(),
                )
            })
    }

    fn dispatch_store(&self) -> Arc<crate::AnyCoordinatorDispatch> {
        self.dispatch.clone()
    }

    fn dispatch_wake(&self) -> Option<Arc<dyn awaken_dispatch_contract::WakeSignal>> {
        None
    }

    fn stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn StreamCheckpointStore>, RuntimeAuthorityError> {
        let mut checkpoints = self.checkpoints.lock().map_err(|_| {
            RuntimeAuthorityError::Corrupt(
                "ephemeral checkpoint authority lock poisoned".to_owned(),
            )
        })?;
        Ok(checkpoints
            .entry(thread.to_owned())
            .or_insert_with(|| {
                Arc::new(awaken_store_inmem::MemoryStreamCheckpointStore::new())
                    as Arc<dyn StreamCheckpointStore>
            })
            .clone())
    }
}
