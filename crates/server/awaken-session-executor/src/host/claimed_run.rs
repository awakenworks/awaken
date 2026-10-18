// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
// Original Run assembly consumes the already-admitted dispatch and its unchanged commit, permission and credential owners.
//! Reconstructs the existing child Run executor from its frozen inputs.
use super::*;
use awaken_dispatch_execution::{DispatchWorker, Error};
pub(super) async fn construct_claimed_child(
    host: &crate::RunAssembly,
    claimed: &awaken_dispatch_contract::Claimed,
    session_thread_id: &awaken_agent_contract::agent::thread::Id,
    publication_source: Arc<awaken_runtime_contract::StaticPublishedAgentSnapshots>,
) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
    crate::agent_runner::validate_child_tool_configuration(
        &claimed.request.activation.snapshot,
        claimed.request.activation.inherited_toolsets(),
    )
    .map_err(|error| {
        awaken_dispatch_execution::session_execution::execution_error(error.to_string())
    })?;
    let substrate = host
        .session_child_execution_substrate(&session_thread_id.0, None)
        .await
        .map_err(awaken_dispatch_execution::session_worker_resolver::map_admission_error)?;
    let environment = substrate.environment;
    let snapshot = &claimed.request.activation.snapshot;
    let authorization = crate::config::effective_tool_authorization(
        &snapshot.resolved_spec.plugin_config,
        &[],
        &snapshot.resolved_spec.plugin_config.agent.toolsets,
    );
    let runtime = crate::config::build_runtime_with_authorization(
        host.agent.llm.clone(),
        host.clock.clone(),
        environment.as_ref(),
        &authorization,
        Some(host.background_supervisor.clone()),
    );
    let commit = substrate.commit;
    let adapters = host.child_execution_adapters(&session_thread_id.0, environment.clone());
    let mut runtime = crate::agent_runner::configure_child_native_runtime(
        runtime,
        snapshot,
        claimed.request.activation.inherited_toolsets(),
        &adapters,
    )
    .map_err(|error| {
        awaken_dispatch_execution::session_execution::execution_error(error.to_string())
    })?;
    if let Some(delegation) = host
        .run_delegation(
            &session_thread_id.0,
            environment.clone(),
            commit.clone(),
            Some(snapshot),
            publication_source,
            crate::delegate::CallerToolConfiguration {
                toolsets: snapshot.resolved_spec.plugin_config.agent.toolsets.clone(),
                inherited: claimed.request.activation.inherited_toolsets().cloned(),
            },
        )
        .map_err(awaken_dispatch_execution::session_worker_resolver::map_admission_error)?
    {
        runtime = runtime.with_run_delegation(delegation);
    }
    let runtime = Arc::new(runtime);
    let attempt = crate::agent_runner::child_attempt_executor(
        runtime,
        snapshot,
        &adapters,
        authorization.policy,
    )
    .map_err(|error| {
        awaken_dispatch_execution::session_execution::execution_error(error.to_string())
    })?;
    crate::agent_runner::ChildWorkerFactory::claimed_recovery(
        host,
        claimed,
        commit,
        session_thread_id,
        crate::agent_runner::ClaimedChildAttempt {
            executor: attempt,
            context: substrate.attempt_context,
            credentials: adapters.credential_capabilities,
        },
    )
    .await
}
