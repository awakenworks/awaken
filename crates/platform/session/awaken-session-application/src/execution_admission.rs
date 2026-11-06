// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Freezes required execution observations and compares canonical instance identity
// @formal reason: before composition effects. It reads no service facts and issues no execution
// @formal reason: authority; construction decision tables test exact/foreign and partial bindings.
//! Required execution observations frozen before the Session application is shared.

use awaken_dispatch_contract::DispatchOperationalFeed;
use awaken_session_execution_contract::SessionPreparationPlacement;
use awaken_worker_contract::WorkerIncarnationSource;
use std::sync::Arc;

/// The dispatch feed, exact Registry reads and placement compiler used by one process.
/// Every construction supplies the complete value before the App is shared.
/// Test-support callers may explicitly select the existing recording Probe;
/// this value carries read observations and a pure compiler, not execution rights.
///
/// ```compile_fail,E0061
/// use std::sync::Arc;
/// use awaken_session_application::SessionExecutionAdmission;
/// use awaken_dispatch_contract::DispatchOperationalFeed;
/// use awaken_worker_contract::WorkerIncarnationSource;
/// fn incomplete(dispatch: Arc<dyn DispatchOperationalFeed>, workers: Arc<dyn WorkerIncarnationSource>) {
///     let _ = SessionExecutionAdmission::new(dispatch, workers);
/// }
/// ```
///
/// Roster-only access cannot supply historical predecessor exit evidence:
///
/// ```compile_fail,E0308
/// use std::sync::Arc;
/// use awaken_session_application::SessionExecutionAdmission;
/// use awaken_dispatch_contract::DispatchOperationalFeed;
/// use awaken_worker_contract::WorkerObservationSource;
/// use awaken_session_execution_contract::SessionPreparationPlacement;
/// fn roster_only(
///     dispatch: Arc<dyn DispatchOperationalFeed>, workers: Arc<dyn WorkerObservationSource>,
///     placement: Arc<dyn SessionPreparationPlacement>,
/// ) {
///     let _ = SessionExecutionAdmission::new(dispatch, workers, placement);
/// }
/// ```
///
/// ```
/// use std::sync::Arc;
/// use awaken_session_application::SessionExecutionAdmission;
/// use awaken_dispatch_contract::DispatchOperationalFeed;
/// use awaken_worker_contract::WorkerIncarnationSource;
/// use awaken_session_execution_contract::SessionPreparationPlacement;
/// fn complete(
///     dispatch: Arc<dyn DispatchOperationalFeed>, workers: Arc<dyn WorkerIncarnationSource>,
///     placement: Arc<dyn SessionPreparationPlacement>,
/// ) -> SessionExecutionAdmission {
///     SessionExecutionAdmission::new(dispatch, workers, placement)
/// }
/// ```
#[cfg_attr(not(feature = "test-support"), doc = include_str!("execution_admission_production.md"))]
pub struct SessionExecutionAdmission {
    pub(super) dispatch: Arc<dyn DispatchOperationalFeed>,
    pub(super) workers: Arc<dyn WorkerIncarnationSource>,
    pub(super) placement: Arc<dyn SessionPreparationPlacement>,
}

#[cfg(any(test, feature = "test-support"))]
mod test_support;
#[cfg(any(test, feature = "test-support"))]
pub use test_support::ExecutionObservationProbe;

impl SessionExecutionAdmission {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn DispatchOperationalFeed>,
        workers: Arc<dyn WorkerIncarnationSource>,
        placement: Arc<dyn SessionPreparationPlacement>,
    ) -> Self {
        Self {
            dispatch,
            workers,
            placement,
        }
    }

    pub fn verify_dispatch(
        &self,
        dispatch: &Arc<dyn DispatchOperationalFeed>,
    ) -> Result<(), &'static str> {
        require_same(
            &self.dispatch,
            dispatch,
            "Session dispatch observation source differs from the canonical Runtime",
        )
    }

    pub fn verify_worker_admission(
        &self,
        workers: &Arc<dyn WorkerIncarnationSource>,
        placement: &Arc<dyn SessionPreparationPlacement>,
    ) -> Result<(), &'static str> {
        require_same(
            &self.workers,
            workers,
            "Session Worker observation source differs from the canonical registry",
        )?;
        require_same(
            &self.placement,
            placement,
            "Session preparation compiler differs from the canonical Host",
        )
    }
}

impl SessionExecutionAdmission {
    /// Read the canonical Registry's recorded end of the incarnation holding
    /// the Resident source. Construction has already selected the exact reader;
    /// an unrecorded end remains an observation result, never a missing role.
    pub(crate) async fn projection_for_worker(
        &self,
        queries: &crate::SessionQueries,
        owner_scope: String,
        session: &awaken_session_contract::PersistedSession,
        activate_pending_resources: bool,
    ) -> Result<
        awaken_session_execution_contract::FrozenSessionProjection,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        let mut projection = queries
            .frozen_runtime_projection(owner_scope, session, true, activate_pending_resources)
            .await
            .map_err(crate::realization::unavailable)?;
        projection.bound_source_end =
            awaken_session_execution_contract::BoundSourceEnd::read(session, self.workers.as_ref())
                .await
                .map_err(crate::realization::unavailable)?;
        Ok(projection)
    }
}

fn require_same<T: ?Sized>(
    actual: &Arc<T>,
    expected: &Arc<T>,
    error: &'static str,
) -> Result<(), &'static str> {
    Arc::ptr_eq(actual, expected).then_some(()).ok_or(error)
}
