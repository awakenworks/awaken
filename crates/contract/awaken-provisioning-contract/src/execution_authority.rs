//! Synchronous authority port for committing a physical execution effect.

use std::{io, time::Duration};

/// Invoke the effect exactly once while holding the SAME synchronous authority
/// guard used to revoke or replace the current generation. The remaining
/// lifetime comes from current authority, never a copied creation fence.
/// Denial invokes no callback; the callback must perform no asynchronous work.
/// Composition may intersect Worker and Session authority through this port.
pub trait CurrentExecutionAuthority: Send + Sync {
    fn with_current(&self, effect: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()>;

    /// Commit an already admitted provider effect under the same guards. The
    /// implementation must bind the asserted operation and generation, reject
    /// future/regressed assertions, and supply the current same-effect fence.
    /// A generic execution grant cannot certify an arbitrary provider effect.
    /// The callback is synchronous and runs exactly once, only on admission.
    fn with_current_effect(
        &self,
        _asserted: &crate::SandboxEffectFence,
        _effect: &mut dyn FnMut(&crate::SandboxEffectFence, Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "execution authority is not bound to a provider effect",
        ))
    }
}

/// Run one synchronous Sandbox effect under its current operation-bound guard.
/// Shared by Memory acknowledgement/preparation and physical disposal. Durable
/// expiry stays in Control coordinates; this edge consumes the current local
/// proof. Re-enter after asynchronous waits: success cannot authorize an
/// in-flight remote commit or a later effect. Faulty ports cannot omit/repeat
/// the callback, widen its identity, or turn an after-effect error into success.
pub fn with_current_sandbox_effect<T>(
    asserted: &crate::SandboxEffectFence,
    authority: &dyn CurrentExecutionAuthority,
    apply: impl FnOnce() -> Result<T, crate::SandboxError>,
) -> Result<T, crate::SandboxError> {
    asserted.validate_identity()?;
    let mut apply = Some(apply);
    let mut result = None;
    authority
        .with_current_effect(asserted, &mut |current, remaining| {
            if remaining.is_zero()
                || !asserted.same_effect_identity(current)
                || current.expires_at_unix_ms < asserted.expires_at_unix_ms
            {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            let apply = apply.take().ok_or(io::ErrorKind::PermissionDenied)?;
            result = Some(apply().map_err(io::Error::other)?);
            Ok(())
        })
        .map_err(|error| crate::SandboxError::new(error.to_string()))?;
    result.ok_or_else(|| crate::SandboxError::new("Sandbox authority omitted the effect"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_only_authority_cannot_certify_an_unbound_provider_effect() {
        // D1 generic current execution grant exists, D2 no operation binding:
        // plain execution runs once; scoped provider admission denies with
        // zero callbacks. The default must not infer scope from caller bytes.
        struct ExecutionOnly;
        impl CurrentExecutionAuthority for ExecutionOnly {
            fn with_current(
                &self,
                effect: &mut dyn FnMut(Duration) -> io::Result<()>,
            ) -> io::Result<()> {
                effect(Duration::from_secs(1))
            }
        }
        ExecutionOnly.with_current(&mut |_| Ok(())).unwrap();
        let fence = crate::SandboxEffectFence::new("op", "owner", "boot", 1, 100).unwrap();
        assert!(
            ExecutionOnly
                .with_current_effect(&fence, &mut |_, _| {
                    panic!("D2 no operation-bound authority")
                })
                .is_err()
        );
    }
}
