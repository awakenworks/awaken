// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Worker proof composition at existing Session authority installation seams.

use std::io;

impl crate::SharedHost {
    /// Lock order: Worker proof -> Session slots -> synchronous provider grant.
    /// No async owner lock or external I/O may be held by this callback.
    pub(crate) fn authorize_worker_effect<T>(
        &self,
        effect: impl FnOnce() -> Result<T, crate::HostError>,
    ) -> Result<T, crate::HostError> {
        let Some(authority) = &self.realizer.execution.worker else {
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
