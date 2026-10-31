// @formal entrypoint session_execution.terminal_state_never_reopens
// Existing SDK observation and cancellation capabilities configure the shared Session-bound Worker.
//! SDK-only observation and attempt capabilities around the shared commit factory.
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// Select the original SDK raw commit and recovery sources. The shared factory
/// owns the one open/Worker implementation; only SDK observation stays here.
pub(crate) fn commit_boundary_workers(
    assembly: &crate::RunAssembly,
) -> awaken_session_run_delivery::SessionCommitWorkers {
    awaken_session_run_delivery::SessionCommitWorkers::new(
        assembly.authority.clone(),
        assembly.authority.dispatch_binding(),
        assembly.clock.clone(),
        assembly.thread_reads.clone(),
        Arc::new(EmbeddedBoundaryObservation {
            observes_terminal_memory_locally: assembly.authority.observes_terminal_memory_locally(),
            capabilities: assembly.capabilities.clone(),
            memory_terminal_observers: assembly.resources.memory_terminal_observers.clone(),
        }),
    )
}

/// The SDK's original late observer selection; it holds no Run assembly or
/// claimed-execution factory. The selected collaborators keep their own state.
struct EmbeddedBoundaryObservation {
    observes_terminal_memory_locally: bool,
    capabilities: Arc<crate::RunCapabilities>,
    memory_terminal_observers: Arc<crate::memory::MemoryTerminalObservers>,
}

#[async_trait::async_trait]
impl awaken_session_run_delivery::SessionBoundaryEffects for EmbeddedBoundaryObservation {
    async fn configure(
        self: Arc<Self>,
        mut worker: awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>,
        claimed: &awaken_dispatch_contract::Claimed,
        commit: Arc<awaken_session_run_delivery::SessionCommit>,
        mut context: Option<awaken_runtime_contract::RuntimeRunContext>,
    ) -> Result<
        awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>,
        awaken_dispatch_execution::Error,
    > {
        // Select through the original shared binding now, after Session assembly;
        // constructing the resolver must not cache an absent or expired observer.
        if let Some(observer) = self.capabilities.dispatch_settlement_observer() {
            worker = worker
                .with_settlement_observer(observer.clone())
                .with_session_run_admission_observer(observer);
        }
        if self.observes_terminal_memory_locally
            && let Some(observer) = self
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
        Ok(worker)
    }
}

/// Only the claiming Worker may install a remote attempt executor. The shared
/// read/repair factory exposes no cancellation-capability selector.
pub(super) async fn cancellation_worker(
    assembly: &crate::RunAssembly,
    claimed: &awaken_dispatch_contract::Claimed,
) -> Result<
    awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>,
    awaken_dispatch_execution::Error,
> {
    let boundaries = commit_boundary_workers(assembly);
    let commit = boundaries
        .open_session_commit(claimed.request.session_thread_id())
        .await?;
    let worker = boundaries.worker(claimed, commit, None).await?;
    if matches!(
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
    ) {
        let executor = assembly
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
    Ok(worker)
}
