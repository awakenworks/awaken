// @formal entrypoint session_execution.terminal_state_never_reopens
// The original combined commit, dispatch binding and observer roles remain the only authorities.
//! Environment-free Worker construction over the selected Session commit source.

use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_dispatch_contract::Claimed;
use awaken_dispatch_erasure::{AnyDispatchStore, DispatchBinding};
use awaken_dispatch_execution::session_execution::{
    CommitBoundaryWorkers, ThreadCommitBoundary, execution_error,
};
use awaken_dispatch_execution::{DispatchWorker, Error};
use awaken_runtime_contract::RuntimeRunContext;

use crate::{SessionCommit, SessionCommitSource};

/// Effects selected after the canonical Worker and its recovery reader exist.
/// Consumer: `SessionCommitWorkers`. Implementations: the Coordinator's selected
/// Session observer and the SDK's late coordination/Memory observation adapter.
/// This required role configures existing observers/context only; cancellation
/// and attempt executors stay on the claim-owning Worker's execution path.
#[async_trait::async_trait]
pub trait SessionBoundaryEffects: Send + Sync {
    async fn configure(
        self: Arc<Self>,
        worker: DispatchWorker<AnyDispatchStore>,
        claimed: &Claimed,
        commit: Arc<SessionCommit>,
        context: Option<RuntimeRunContext>,
    ) -> Result<DispatchWorker<AnyDispatchStore>, Error>;
}

/// The one factory for Session commit-boundary Workers. The root selects its
/// original raw source, binding, clock, physical-attempt reader and observation
/// effects once. No Session context, model, provider, new cache or queue is owned.
/// The selected source and effects retain their own original collaborators.
#[derive(Clone)]
pub struct SessionCommitWorkers {
    commits: Arc<dyn SessionCommitSource>,
    binding: Option<DispatchBinding>,
    clock: Arc<dyn awaken_time::WallClock>,
    recovery: Arc<dyn RunRecoverySource>,
    effects: Arc<dyn SessionBoundaryEffects>,
}

impl SessionCommitWorkers {
    #[must_use]
    pub fn new(
        commits: Arc<dyn SessionCommitSource>,
        binding: Option<DispatchBinding>,
        clock: Arc<dyn awaken_time::WallClock>,
        recovery: Arc<dyn RunRecoverySource>,
        effects: Arc<dyn SessionBoundaryEffects>,
    ) -> Self {
        Self {
            commits,
            binding,
            clock,
            recovery,
            effects,
        }
    }

    /// Open the selected raw source at the caller's exact physical Session.
    /// Resident reuse and lifecycle locking belong to a source that selects them.
    pub async fn open_session_commit(
        &self,
        thread: &ThreadId,
    ) -> Result<Arc<SessionCommit>, Error> {
        self.commits
            .open_for_read(&thread.0)
            .await
            .map_err(|error| execution_error(error.message))
    }

    /// Construct over the exact original commit, including a child attempt that
    /// already holds it. The binding refusal precedes effects, then the original
    /// shared constructor installs the recovery reader before observer/context
    /// configuration. This role cannot select a cancellation executor.
    pub async fn worker(
        &self,
        claimed: &Claimed,
        commit: Arc<SessionCommit>,
        context: Option<RuntimeRunContext>,
    ) -> Result<DispatchWorker<AnyDispatchStore>, Error> {
        let binding = self
            .binding
            .clone()
            .ok_or_else(|| execution_error("claimed recovery requires a dispatch binding"))?;
        let worker = crate::session_dispatch_worker(
            Arc::new(awaken_runtime::Runtime::new().with_clock(self.clock.clone())),
            binding,
            commit.clone(),
            claimed.lease.owner().to_owned(),
        )?;
        self.effects
            .clone()
            .configure(worker, claimed, commit, context)
            .await
    }
}

#[async_trait::async_trait]
impl CommitBoundaryWorkers for SessionCommitWorkers {
    async fn open_commit(&self, thread: &ThreadId) -> Result<Box<dyn ThreadCommitBoundary>, Error> {
        let commit = self.open_session_commit(thread).await?;
        Ok(Box::new(OpenedSessionCommit {
            workers: self.clone(),
            commit,
        }))
    }

    fn recovery_source(&self) -> Result<Arc<dyn RunRecoverySource>, Error> {
        Ok(self.recovery.clone())
    }
}

struct OpenedSessionCommit {
    workers: SessionCommitWorkers,
    commit: Arc<SessionCommit>,
}

#[async_trait::async_trait]
impl ThreadCommitBoundary for OpenedSessionCommit {
    fn committed(&self) -> &dyn CommittedThreadView {
        self.commit.as_ref()
    }

    async fn boundary_worker(
        &self,
        claimed: &Claimed,
        context: Option<RuntimeRunContext>,
    ) -> Result<DispatchWorker<AnyDispatchStore>, Error> {
        self.workers
            .worker(claimed, self.commit.clone(), context)
            .await
    }
}

#[cfg(test)]
mod tests;
