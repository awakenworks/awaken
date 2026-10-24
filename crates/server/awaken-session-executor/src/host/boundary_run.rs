// @formal entrypoint session_execution.terminal_state_never_reopens
// Original Run assembly consumes the already-admitted dispatch and its unchanged commit, permission and credential owners.
//! Environment-free Run worker assembly over the original commit boundary.
use super::*;

use awaken_dispatch_execution::session_execution::{CommitBoundaryWorkers, ThreadCommitBoundary};

/// Assembles environment-free commit-boundary Workers from immutable views of
/// the owners they use. Both resolver constructions select it; it holds no
/// Session context, claimed-execution factory, queue or recovery state, so a
/// Coordinator-only resolver retains no Run assembly (and no realization).
#[derive(Clone)]
pub(crate) struct CommitBoundaryAssembly {
    authority: Arc<dyn crate::SessionAuthority>,
    clock: Arc<dyn awaken_time::WallClock>,
    capabilities: Arc<crate::RunCapabilities>,
    memory_terminal_observers: Arc<crate::memory::MemoryTerminalObservers>,
    thread_reads: Arc<crate::ThreadReads>,
}

impl CommitBoundaryAssembly {
    pub(crate) fn new(assembly: &crate::RunAssembly) -> Self {
        Self {
            authority: assembly.authority.clone(),
            clock: assembly.clock.clone(),
            capabilities: assembly.capabilities.clone(),
            memory_terminal_observers: assembly.resources.memory_terminal_observers.clone(),
            thread_reads: assembly.thread_reads.clone(),
        }
    }
}

#[async_trait::async_trait]
impl CommitBoundaryWorkers for CommitBoundaryAssembly {
    async fn open_commit(
        &self,
        thread: &ThreadId,
    ) -> Result<Box<dyn ThreadCommitBoundary>, awaken_dispatch_execution::Error> {
        let commit = self
            .authority
            .open_commit(&thread.0)
            .await
            .map_err(|error| {
                awaken_dispatch_execution::session_execution::execution_error(error.to_string())
            })?;
        Ok(Box::new(OpenedThreadCommit {
            boundaries: self.clone(),
            commit: Arc::new(commit),
        }))
    }

    fn recovery_source(
        &self,
    ) -> Result<
        Arc<dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource>,
        awaken_dispatch_execution::Error,
    > {
        Ok(self.thread_reads.clone())
    }
}

/// One opened original commit, without retaining the claimed Run assembler.
struct OpenedThreadCommit {
    boundaries: CommitBoundaryAssembly,
    commit: Arc<crate::store::HostCommit>,
}

#[async_trait::async_trait]
impl ThreadCommitBoundary for OpenedThreadCommit {
    fn committed(
        &self,
    ) -> &dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView {
        self.commit.as_ref()
    }

    async fn boundary_worker(
        &self,
        claimed: &awaken_dispatch_contract::Claimed,
        context: Option<awaken_runtime_contract::RuntimeRunContext>,
        install_cancellation_executor: bool,
    ) -> Result<
        awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>,
        awaken_dispatch_execution::Error,
    > {
        construct_boundary_worker(
            &self.boundaries,
            claimed,
            self.commit.clone(),
            context,
            install_cancellation_executor,
        )
        .await
    }
}

pub(crate) async fn construct_boundary_worker(
    boundaries: &CommitBoundaryAssembly,
    claimed: &awaken_dispatch_contract::Claimed,
    commit: Arc<crate::store::HostCommit>,
    mut context: Option<awaken_runtime_contract::RuntimeRunContext>,
    install_cancellation_executor: bool,
) -> Result<
    awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>,
    awaken_dispatch_execution::Error,
> {
    let recovery_projection = commit.recovery_projection();
    let binding = boundaries.authority.dispatch_binding().ok_or_else(|| {
        awaken_dispatch_execution::session_execution::execution_error(
            "claimed recovery requires a dispatch binding",
        )
    })?;
    let mut worker = awaken_dispatch_execution::DispatchWorker::new_bound(
        Arc::new(awaken_runtime::Runtime::new().with_clock(boundaries.clock.clone())),
        binding,
        commit.clone(),
        claimed.lease.owner().to_owned(),
    );
    // Select through the original shared binding now, after Session assembly;
    // constructing the resolver must not cache an absent or expired observer.
    if let Some(observer) = boundaries.capabilities.dispatch_settlement_observer() {
        worker = worker
            .with_settlement_observer(observer.clone())
            .with_session_run_admission_observer(observer);
    }
    if boundaries.authority.observes_terminal_memory_locally()
        && let Some(observer) = boundaries
            .memory_terminal_observers
            .for_dispatch(&claimed.request, commit.clone())
            .await
            .map_err(|error| {
                awaken_dispatch_execution::session_execution::execution_error(error.to_string())
            })?
    {
        // Child execution supplies its attempt context here. Adding the
        // exact logical Run observer before the single `with_context`
        // prevents a later child install from erasing terminal Memory work.
        context = Some(context.unwrap_or_default().with_terminal_observer(observer));
    }
    if let Some(context) = context {
        worker = worker.with_context(context);
    }
    if install_cancellation_executor
        && matches!(
            awaken_runtime_contract::resolved::Backend::from_ref(
                claimed
                    .request
                    .activation
                    .snapshot
                    .resolved_spec
                    .model_binding
                    .identity()
                    .2
            ),
            awaken_runtime_contract::resolved::Backend::Remote(_)
        )
    {
        let executor = boundaries
            .capabilities
            .agent
            .remote_attempt_executor
            .clone()
            .ok_or_else(|| {
                awaken_dispatch_execution::session_execution::execution_error(
                    "remote cancellation requires a configured remote attempt executor",
                )
            })?;
        worker.install_attempt_executor(executor);
    }

    if let Some(projection) = recovery_projection {
        worker = worker.with_recovery_projection(projection)?;
    }
    Ok(worker)
}
