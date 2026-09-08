//! Synchronous authority port for committing a physical execution effect.

use std::{io, time::Duration};

/// Invoke the effect exactly once while holding the SAME synchronous authority
/// guard used to revoke or replace the current generation. The remaining
/// lifetime comes from current authority, never a copied creation fence.
/// Denial invokes no callback; the callback must perform no asynchronous work.
/// Composition may intersect Worker and Session authority through this port.
pub trait CurrentExecutionAuthority: Send + Sync {
    fn with_current(&self, effect: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()>;

    /// Snapshot the current same-effect proof under the canonical guards.
    /// This is a retry projection, not authority for a later physical effect;
    /// re-enter the guarded port after asynchronous work.
    fn current_effect_fence(
        &self,
        asserted: &crate::SandboxEffectFence,
    ) -> Result<crate::SandboxEffectFence, crate::SandboxError> {
        with_checked_effect(asserted, self, |current| Ok(current.clone()))
    }

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
    with_checked_effect(asserted, authority, |_| apply())
}

fn with_checked_effect<T, A: CurrentExecutionAuthority + ?Sized>(
    asserted: &crate::SandboxEffectFence,
    authority: &A,
    apply: impl FnOnce(&crate::SandboxEffectFence) -> Result<T, crate::SandboxError>,
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
            result = Some(apply(current).map_err(io::Error::other)?);
            Ok(())
        })
        .map_err(|error| crate::SandboxError::new(error.to_string()))?;
    result.ok_or_else(|| crate::SandboxError::new("Sandbox authority omitted the effect"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_and_effect_share_checked_admission() {
        // Causes: C1 callback supplied/omitted/repeated; C2 same/foreign
        // identity; C3 equal/renewed/regressed durable expiry; C4 positive/zero
        // local budget; C5 port success/error after callback. Rules: only
        // exactly one callback + same identity + nonregressed expiry + positive
        // budget + successful port returns success. Invalid proof/omission
        // executes zero effects; repeat/error cannot manufacture success or
        // execute a second effect. Durable expiry=100 is deliberately unrelated
        // to this machine's UTC. Both APIs use the same decision table.
        struct Port {
            fence: crate::SandboxEffectFence,
            remaining: Duration,
            calls: usize,
            fail_after: bool,
        }
        impl CurrentExecutionAuthority for Port {
            fn with_current(
                &self,
                _: &mut dyn FnMut(Duration) -> io::Result<()>,
            ) -> io::Result<()> {
                panic!("provider effects must not use generic admission")
            }
            fn with_current_effect(
                &self,
                _: &crate::SandboxEffectFence,
                effect: &mut dyn FnMut(&crate::SandboxEffectFence, Duration) -> io::Result<()>,
            ) -> io::Result<()> {
                for _ in 0..self.calls {
                    effect(&self.fence, self.remaining)?;
                }
                if self.fail_after {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(())
                }
            }
        }
        let initial = crate::SandboxEffectFence::new("op", "owner", "boot", 1, 100).unwrap();
        for (rule, expiry, foreign, budget, calls, fail_after, admitted, effects) in [
            ("equal", 100, false, 1, 1, false, true, 1),
            ("renewed", 200, false, 1, 1, false, true, 1),
            ("regressed", 99, false, 1, 1, false, false, 0),
            ("foreign", 200, true, 1, 1, false, false, 0),
            ("zero", 100, false, 0, 1, false, false, 0),
            ("omitted", 100, false, 1, 0, false, false, 0),
            ("repeated", 100, false, 1, 2, false, false, 1),
            ("error_after", 100, false, 1, 1, true, false, 1),
        ] {
            let mut fence = initial.clone();
            fence.expires_at_unix_ms = expiry;
            if foreign {
                fence.operation_id = "other".into();
            }
            let port = Port {
                fence,
                remaining: Duration::from_secs(budget),
                calls,
                fail_after,
            };
            let snapshot = (&port as &dyn CurrentExecutionAuthority).current_effect_fence(&initial);
            assert_eq!(snapshot.is_ok(), admitted, "{rule}");
            if admitted {
                assert_eq!(snapshot.unwrap(), port.fence, "{rule}");
            }
            let mut count = 0;
            let result = with_current_sandbox_effect(&initial, &port, || {
                count += 1;
                Ok(())
            });
            assert_eq!(result.is_ok(), admitted, "{rule}");
            assert_eq!(count, effects, "{rule}");
        }
    }

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
        assert!(ExecutionOnly.current_effect_fence(&fence).is_err());
        assert!(
            ExecutionOnly
                .with_current_effect(&fence, &mut |_, _| {
                    panic!("D2 no operation-bound authority")
                })
                .is_err()
        );
    }
}
