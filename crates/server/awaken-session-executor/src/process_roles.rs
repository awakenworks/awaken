// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Worker source-retention observations reuse the Environment owner's source identity
// @formal reason: law, exact local Arc and handle, original provider proof and generation-scoped
// @formal reason: BackgroundRuns activity. Source-retention decision tables exercise this adapter;
// @formal reason: provider/storage facts and async scheduling remain external proof boundaries.
// Startup capability lifetimes are owned by the composition root's lifecycle.
//! Claiming-Worker roles over the executor's selected components.

use std::sync::Arc;

use awaken_dispatch_contract::CompletionSink;
use awaken_dispatch_erasure::AnyDispatchStore;
use awaken_dispatch_execution::{ProcessDispatch, WorkerResolver};
use awaken_session_execution_contract::{
    SessionApplicationBindingError, SessionApplicationLifetime, SessionApplications,
};
use awaken_session_realization::peers::EnvironmentPeer as _;
use awaken_session_run_delivery::SessionDispatchLifecycle;

/// An execution service handle that owns every selected application port.
pub type SessionExecutionLifecycle = SessionDispatchLifecycle<SessionApplicationLifetime>;

/// The execution roles a database-less Worker composition selects.
#[derive(Clone)]
pub struct WorkerRoles {
    lifecycle: SessionExecutionLifecycle,
    assembly: Arc<crate::RunAssembly>,
    runs: Arc<crate::SessionRuns>,
}

/// Complete the executor's one Session application binding and select the
/// shared process lifecycle over the executor's original authority, lazy
/// resolver and auxiliary registry. This is the only place a composition root
/// binds the application; Coordinator-only roots construct the same component
/// directly, without an executor and without a binding.
///
/// Execution cannot start through the old unchecked Host-only constructor:
///
/// ```compile_fail,E0061
/// use std::sync::Arc;
/// use awaken_session_executor::{ExecutorComponents, session_dispatch_lifecycle};
/// use awaken_dispatch_contract::CompletionSink;
/// fn unchecked(host: &ExecutorComponents, sink: Arc<dyn CompletionSink>) {
///     let _ = session_dispatch_lifecycle(host, sink);
/// }
/// ```
pub fn session_dispatch_lifecycle(
    host: &crate::ExecutorComponents,
    completion: Arc<dyn CompletionSink>,
    applications: SessionApplications,
) -> Result<SessionExecutionLifecycle, SessionApplicationBindingError> {
    use SessionApplicationBindingError as E;
    use awaken_session_execution_contract::SessionApplicationMode as M;
    if host.dispatch.has_started_services() {
        return Err(E::ServicesAlreadyStarted);
    }
    let local = host.run_assembly.authority.hosts_session_application();
    match applications.mode() {
        M::Local | M::Standalone if !local => {
            return Err(E::InvalidTopology(
                "local applications require a local authority",
            ));
        }
        M::ClaimedWorker if local => {
            return Err(E::InvalidTopology(
                "claimed Worker requires an upstream authority",
            ));
        }
        M::ClaimedWorker
            if host.capabilities.worker.session_control.is_none()
                || host
                    .capabilities
                    .worker
                    .session_coordination_control
                    .is_none() =>
        {
            return Err(E::RequiredApplicationUnavailable(
                "claimed Session realization and coordination controls",
            ));
        }
        _ => {}
    }
    // Keep strong owners outside the publication callback. Even a rejected
    // custom port is dropped only after the process configuration guard ends.
    let lifetime = host
        .dispatch
        .configure_before_services_start(|| host.session_application.bind(&applications))
        .ok_or(E::ServicesAlreadyStarted)??;
    Ok(retained_lifecycle(host, completion, lifetime))
}

#[cfg(test)]
pub(crate) fn selected_test_lifecycle(
    host: &crate::ExecutorComponents,
    completion: Arc<dyn CompletionSink>,
) -> Result<SessionExecutionLifecycle, SessionApplicationBindingError> {
    Ok(retained_lifecycle(
        host,
        completion,
        host.session_application.acquire()?,
    ))
}

fn retained_lifecycle(
    host: &crate::ExecutorComponents,
    completion: Arc<dyn CompletionSink>,
    lifetime: SessionApplicationLifetime,
) -> SessionExecutionLifecycle {
    let assembly = host.run_assembly.clone();
    let authority = assembly.authority.as_ref();
    let terminal_owner = authority
        .scans_committed_terminals()
        .then(|| crate::host::capability::local_dispatch_owner(authority))
        .flatten();
    SessionDispatchLifecycle::new(
        host.dispatch.clone(),
        authority.dispatch_binding(),
        completion,
        terminal_owner,
        host.resources.memory.background(),
        move |dispatch| {
            crate::host::session_worker_resolver(&assembly, dispatch)
                .map(|resolver| Arc::new(resolver) as Arc<dyn WorkerResolver<AnyDispatchStore>>)
                .map_err(|error| {
                    awaken_dispatch_execution::session_execution::execution_error(error.message)
                })
        },
    )
    .retaining(lifetime)
}

/// Drain this Host's existing claimed and auxiliary work without selecting
/// applications or changing already-started service construction ports.
pub async fn drain_session_dispatch(
    host: &crate::ExecutorComponents,
    timeout: std::time::Duration,
) -> bool {
    awaken_session_run_delivery::drain_session_dispatch(
        &host.dispatch,
        &host.resources.memory.background(),
        timeout,
    )
    .await
}

impl WorkerRoles {
    /// Select the complete application topology at the composition root.
    /// Local, standalone and claimed execution share this same mechanism.
    pub fn of(
        host: &crate::ExecutorComponents,
        applications: SessionApplications,
    ) -> Result<Self, SessionApplicationBindingError> {
        Ok(Self {
            lifecycle: session_dispatch_lifecycle(
                host,
                host.worker_completion_sink(),
                applications,
            )?,
            assembly: host.run_assembly.clone(),
            runs: host.session_runs.clone(),
        })
    }

    /// This Worker's dispatch: pool, admission and readiness.
    #[must_use]
    pub fn dispatch(&self) -> &Arc<ProcessDispatch> {
        self.lifecycle.dispatch()
    }

    /// Session context assembly: realization renewal, revocation and
    /// preparation the Worker lifecycle drives.
    #[must_use]
    pub fn run_assembly(&self) -> &Arc<crate::RunAssembly> {
        &self.assembly
    }

    /// Session Run delivery: the resident Runs a revocation interrupts.
    #[must_use]
    pub fn session_runs(&self) -> &Arc<crate::SessionRuns> {
        &self.runs
    }

    /// The warm Environment capacity of this Worker's providers.
    #[must_use]
    pub fn environment_capacity(&self) -> &crate::environment_providers::EnvironmentProviders {
        self.assembly
            .realization_executor
            .environments()
            .providers()
    }

    /// Observe whether the original source of one Session this Worker holds
    /// is retained, for the retention report a draining Worker sends. Only
    /// the Worker role set observes Sandboxes; the root supplies preparation
    /// status, and this read installs no slot and grants no preparation,
    /// disposal, retirement or cleanup receipt.
    pub async fn observe_source_retention(
        &self,
        session_id: &awaken_session_types::SessionId,
        projection: &awaken_session_execution_contract::FrozenSessionProjection,
        source_prepared: bool,
    ) -> Result<bool, awaken_runtime_contract::runs::RunError> {
        // Provider retention is not quiescence of this Actor. Observe the same
        // original owner on both sides of the async read; its one activity key
        // also makes an unreadable BackgroundRuns registry refuse retirement.
        let original = self.source_retention_owner(session_id)?;
        if let Some(original) = original.as_ref()
            && !original
                .matches_source_projection(
                    &projection.environment,
                    &projection.workspace_id,
                    session_id.as_str(),
                )
                .map_err(awaken_runtime_contract::runs::RunError::from)?
        {
            return Ok(false);
        }
        // exact_matches may compare the same mutable Arc twice. Capture the
        // SandboxHandle VALUE before suspension, not another view of that Arc.
        let original_handle = original
            .as_ref()
            .map(|owner| owner.environment.lifecycle().handle());
        if self.source_retention_background_busy(session_id, original.as_ref()) {
            return Ok(false);
        }
        let retained = self
            .assembly
            .realization_executor
            .environments()
            .observe_source_retention(session_id, projection, source_prepared)
            .await
            .map_err(awaken_runtime_contract::runs::RunError::from)?;
        if !retained {
            return Ok(false);
        }
        let current = self.source_retention_owner(session_id)?;
        let same_owner = match (&original, &current) {
            (Some(original), Some(current)) => {
                original.exact_matches(current)
                    && original_handle.as_ref() == Some(&current.environment.lifecycle().handle())
            }
            (None, None) => true,
            _ => false,
        };
        Ok(same_owner && !self.source_retention_background_busy(session_id, current.as_ref()))
    }

    fn source_retention_owner(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<
        Option<awaken_session_realization::environment_owner::BoundSessionEnvironment>,
        awaken_runtime_contract::runs::RunError,
    > {
        self.assembly
            .realization_executor
            .sessions()
            .read_realization(session_id.as_str(), |realization| {
                realization.environment().owner.terminal_bound_environment()
            })
            .map(Option::flatten)
            .map_err(awaken_runtime_contract::runs::RunError::from)
    }

    fn source_retention_background_busy(
        &self,
        session_id: &awaken_session_types::SessionId,
        owner: Option<&awaken_session_realization::environment_owner::BoundSessionEnvironment>,
    ) -> bool {
        owner.is_some_and(|owner| {
            self.assembly
                .resources
                .memory
                .background()
                .has_shared_environment_work(session_id.as_str(), &owner.activity_generation_id())
        })
    }

    /// The process wall clock this Worker drives its claims on.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.assembly.clock()
    }

    /// Spawn the Worker's dispatch pool once its upstream authority is bound.
    pub fn start_pool(&self) -> Result<(), awaken_dispatch_execution::Error> {
        self.lifecycle.start_local_pool()
    }

    /// Drain claimed Runs, then the auxiliary work they created, by `timeout`.
    pub async fn drain(&self, timeout: std::time::Duration) -> bool {
        self.lifecycle.drain(timeout).await
    }
}

#[cfg(test)]
mod tests;
