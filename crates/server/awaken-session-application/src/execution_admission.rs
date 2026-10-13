// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Freezes required execution observations and compares canonical instance identity
// @formal reason: before composition effects. It reads no service facts and issues no execution
// @formal reason: authority; construction decision tables test exact/foreign and partial bindings.
//! Required execution observations frozen before the Session application is shared.

use awaken_dispatch_contract::DispatchOperationalFeed;
use awaken_session_realization_contract::SessionPreparationPlacement;
use awaken_worker_contract::WorkerObservationSource;
use std::sync::{Arc, OnceLock};

/// The dispatch feed, Worker roster and placement compiler used by one process.
/// Every production construction supplies the complete value.
///
/// ```compile_fail,E0061
/// use std::sync::Arc;
/// use awaken_session_application::SessionExecutionAdmission;
/// use awaken_dispatch_contract::DispatchOperationalFeed;
/// use awaken_worker_contract::WorkerObservationSource;
/// fn incomplete(dispatch: Arc<dyn DispatchOperationalFeed>, workers: Arc<dyn WorkerObservationSource>) {
///     let _ = SessionExecutionAdmission::new(dispatch, workers);
/// }
/// ```
///
/// ```
/// use std::sync::Arc;
/// use awaken_session_application::SessionExecutionAdmission;
/// use awaken_dispatch_contract::DispatchOperationalFeed;
/// use awaken_worker_contract::WorkerObservationSource;
/// use awaken_session_realization_contract::SessionPreparationPlacement;
/// fn complete(
///     dispatch: Arc<dyn DispatchOperationalFeed>, workers: Arc<dyn WorkerObservationSource>,
///     placement: Arc<dyn SessionPreparationPlacement>,
/// ) -> SessionExecutionAdmission {
///     SessionExecutionAdmission::new(dispatch, workers, placement)
/// }
/// ```
#[cfg_attr(not(feature = "test-support"), doc = include_str!("execution_admission_production.md"))]
pub struct SessionExecutionAdmission {
    pub(super) dispatch: OnceLock<Arc<dyn DispatchOperationalFeed>>,
    pub(super) workers: OnceLock<Arc<dyn WorkerObservationSource>>,
    pub(super) placement: OnceLock<Arc<dyn SessionPreparationPlacement>>,
}

impl SessionExecutionAdmission {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn DispatchOperationalFeed>,
        workers: Arc<dyn WorkerObservationSource>,
        placement: Arc<dyn SessionPreparationPlacement>,
    ) -> Self {
        Self {
            dispatch: OnceLock::from(dispatch),
            workers: OnceLock::from(workers),
            placement: OnceLock::from(placement),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn fixture() -> Self {
        Self {
            dispatch: OnceLock::new(),
            workers: OnceLock::new(),
            placement: OnceLock::new(),
        }
    }

    /// Partial scenario assembly; production cannot construct missing services.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn fixture_with_dispatch(dispatch: Arc<dyn DispatchOperationalFeed>) -> Self {
        Self {
            dispatch: OnceLock::from(dispatch),
            ..Self::fixture()
        }
    }

    #[cfg(test)]
    pub fn set_worker_observation_source(
        &self,
        source: Arc<dyn WorkerObservationSource>,
    ) -> Result<(), &'static str> {
        self.workers
            .set(source)
            .map_err(|_| "Worker observation source is already installed")
    }
    #[cfg(test)]
    pub fn set_dispatch_observation_source(
        &self,
        source: Arc<dyn DispatchOperationalFeed>,
    ) -> Result<(), &'static str> {
        self.dispatch
            .set(source)
            .map_err(|_| "Session dispatch observation source is already installed")
    }
    #[cfg(test)]
    pub fn set_session_preparation_placement(
        &self,
        source: Arc<dyn SessionPreparationPlacement>,
    ) -> Result<(), &'static str> {
        self.placement
            .set(source)
            .map_err(|_| "Session preparation placement is already installed")
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
        workers: &Arc<dyn WorkerObservationSource>,
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn complete_fixture_worker_admission(
        &self,
        workers: Arc<dyn WorkerObservationSource>,
        placement: Arc<dyn SessionPreparationPlacement>,
    ) -> Result<(), &'static str> {
        // Refuse an existing mismatch before filling either missing slot.
        if self
            .workers
            .get()
            .is_some_and(|actual| !Arc::ptr_eq(actual, &workers))
            || self
                .placement
                .get()
                .is_some_and(|actual| !Arc::ptr_eq(actual, &placement))
        {
            return Err("fixture Worker admission differs from its existing bindings");
        }
        self.workers.get_or_init(|| workers.clone());
        self.placement.get_or_init(|| placement.clone());
        self.verify_worker_admission(&workers, &placement)
    }
}

fn require_same<T: ?Sized>(
    slot: &OnceLock<Arc<T>>,
    expected: &Arc<T>,
    error: &'static str,
) -> Result<(), &'static str> {
    slot.get()
        .filter(|actual| Arc::ptr_eq(actual, expected))
        .map(|_| ())
        .ok_or(error)
}
