// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Worker proof composition at existing Session authority installation seams.

use awaken_provisioning_contract::CurrentExecutionAuthority;
use std::{io, sync::Arc};

impl crate::SharedHost {
    /// Composition installs the current registration's proof before dispatch.
    /// An unregistered direct Host keeps its existing Session authority owner;
    /// absence is not evidence for enabling physical process-tree supervision.
    #[must_use]
    pub fn with_execution_authority(
        mut self,
        authority: Arc<dyn CurrentExecutionAuthority>,
    ) -> Self {
        self.execution_authority = Some(authority);
        self
    }

    /// Lock order: Worker proof -> Session slots -> synchronous provider grant.
    /// No async owner lock or external I/O may be held by this callback.
    pub(crate) fn authorize_worker_effect<T>(
        &self,
        effect: impl FnOnce() -> Result<T, crate::HostError>,
    ) -> Result<T, crate::HostError> {
        let Some(authority) = &self.execution_authority else {
            return effect();
        };
        let mut effect = Some(effect);
        let mut result = None;
        authority
            .with_current(&mut |remaining| {
                if remaining.is_zero() {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                let effect = effect
                    .take()
                    .ok_or_else(|| io::Error::other("authority callback repeated"))?;
                result = Some(effect());
                Ok(())
            })
            .map_err(|error| {
                crate::HostError::internal(format!("Worker execution authority denied: {error}"))
            })?;
        result.ok_or_else(|| crate::HostError::internal("Worker authority omitted the effect"))?
    }
}
