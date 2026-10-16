// @formal entrypoint session_execution.terminal_state_never_reopens
// Original Run assembly consumes the already-admitted dispatch and its unchanged commit, permission and credential owners.
//! Environment-free Run worker assembly over the original commit boundary.
use super::*;
pub(crate) async fn construct_boundary_worker(
    host: &SharedHost,
    claimed: &awaken_dispatch_contract::Claimed,
    commit: Arc<crate::store::HostCommit>,
    mut context: Option<awaken_runtime_contract::RuntimeRunContext>,
    install_cancellation_executor: bool,
) -> Result<
    awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>,
    awaken_dispatch_execution::Error,
> {
    let recovery_projection = commit.recovery_projection();
    let binding = host.authority.dispatch_binding().ok_or_else(|| {
        awaken_dispatch_execution::session_execution::execution_error(
            "claimed recovery requires a dispatch binding",
        )
    })?;
    let mut worker = awaken_dispatch_execution::DispatchWorker::new_bound(
        Arc::new(awaken_runtime::Runtime::new().with_clock(host.clock.clone())),
        binding,
        commit.clone(),
        claimed.lease.owner().to_owned(),
    );
    if let Some(observer) = host.dispatch_settlement_observer() {
        worker = worker
            .with_settlement_observer(observer.clone())
            .with_session_run_admission_observer(observer);
    }
    if host.authority.observes_terminal_memory_locally()
        && let Some(observer) = host
            .dispatched_memory_terminal_observer(&claimed.request, commit.clone())
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
        let executor = host.agent.remote_attempt_executor.clone().ok_or_else(|| {
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
