//! Synchronous authority port for committing a physical execution effect.

use std::{io, marker::PhantomData, time::Duration};

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
        with_checked_effect(asserted, self, |guard| Ok(guard.fence().clone()))
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

/// Invariant in `'g` (the function pointer) and `!Send + !Sync` (the raw
/// pointer): the marker a guard carries instead of any runtime state.
type CriticalSection<'g> = PhantomData<(fn(&'g ()) -> &'g (), *const ())>;

/// Proof that the holder is inside the synchronous critical section of one
/// admitted Sandbox effect.
///
/// A guard exists only while [`with_current_sandbox_effect`] runs its callback:
/// the constructor is private to this module and is reached only after the
/// [`CurrentExecutionAuthority`] port has entered the SAME synchronous guard it
/// uses to revoke or replace the generation, and after the current fence was
/// checked to be the asserted effect (identical operation, owner, incarnation
/// and epoch; expiry not regressed; positive remaining budget). The callback
/// receives it by reference under a higher-ranked lifetime, so it cannot be
/// stored, returned, cloned, or sent to another thread; asynchronous work
/// therefore cannot carry it across an await.
///
/// Physical actions that must be fenced take `&SandboxEffectGuard` as a
/// parameter, so calling one outside an admitted effect does not type-check.
/// The guard proves authority only. Providers still perform their own backend
/// compare-and-swap inside it (marker lock, resource version, labels) and
/// compare [`Self::fence`] with the fence recorded in that backend.
///
/// A guard cannot be built outside the admission path:
///
/// ```compile_fail
/// use awaken_provisioning_contract::{SandboxEffectFence, SandboxEffectGuard};
/// let fence = SandboxEffectFence::new("op", "owner", "boot", 1, 100).unwrap();
/// let _forged = SandboxEffectGuard { current: &fence, remaining: std::time::Duration::from_secs(1), _critical_section: std::marker::PhantomData };
/// ```
///
/// Admission hands the guard to a synchronous callback, which passes it on to
/// the fenced action:
///
/// ```
/// # use awaken_provisioning_contract::{CurrentExecutionAuthority, SandboxEffectFence};
/// # use std::{io, time::Duration};
/// # struct Current(SandboxEffectFence);
/// # impl CurrentExecutionAuthority for Current {
/// #     fn with_current(&self, _: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
/// #         Err(io::ErrorKind::PermissionDenied.into())
/// #     }
/// #     fn with_current_effect(
/// #         &self,
/// #         _: &SandboxEffectFence,
/// #         effect: &mut dyn FnMut(&SandboxEffectFence, Duration) -> io::Result<()>,
/// #     ) -> io::Result<()> {
/// #         effect(&self.0, Duration::from_secs(1))
/// #     }
/// # }
/// use awaken_provisioning_contract::{SandboxEffectGuard, SandboxError, with_current_sandbox_effect};
/// fn remove_root(admitted: &SandboxEffectGuard<'_>) -> Result<u64, SandboxError> {
///     Ok(admitted.fence().epoch)
/// }
/// let fence = SandboxEffectFence::new("op", "owner", "boot", 1, 100).unwrap();
/// let epoch = with_current_sandbox_effect(&fence, &Current(fence.clone()), remove_root).unwrap();
/// assert_eq!(epoch, 1);
/// ```
///
/// It cannot escape the critical section that admitted it:
///
/// ```compile_fail
/// # use awaken_provisioning_contract::{CurrentExecutionAuthority, SandboxEffectFence};
/// # use std::{io, time::Duration};
/// # struct Current(SandboxEffectFence);
/// # impl CurrentExecutionAuthority for Current {
/// #     fn with_current(&self, _: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
/// #         Err(io::ErrorKind::PermissionDenied.into())
/// #     }
/// #     fn with_current_effect(
/// #         &self,
/// #         _: &SandboxEffectFence,
/// #         effect: &mut dyn FnMut(&SandboxEffectFence, Duration) -> io::Result<()>,
/// #     ) -> io::Result<()> {
/// #         effect(&self.0, Duration::from_secs(1))
/// #     }
/// # }
/// use awaken_provisioning_contract::{SandboxEffectGuard, with_current_sandbox_effect};
/// let fence = SandboxEffectFence::new("op", "owner", "boot", 1, 100).unwrap();
/// let mut escaped: Option<&SandboxEffectGuard<'_>> = None;
/// with_current_sandbox_effect(&fence, &Current(fence.clone()), |guard| {
///     escaped = Some(guard);
///     Ok(())
/// })
/// .unwrap();
/// let _ = escaped;
/// ```
///
/// And a fenced physical action cannot be invoked without one:
///
/// ```compile_fail
/// use awaken_provisioning_contract::{SandboxEffectFence, SandboxEffectGuard, SandboxError};
/// fn remove_root(_admitted: &SandboxEffectGuard<'_>) -> Result<(), SandboxError> {
///     Ok(())
/// }
/// // A durable fence is evidence, not admission.
/// let fence = SandboxEffectFence::new("op", "owner", "boot", 1, 100).unwrap();
/// remove_root(&fence).unwrap();
/// ```
pub struct SandboxEffectGuard<'g> {
    current: &'g crate::SandboxEffectFence,
    remaining: Duration,
    // Invariant in 'g and neither Send nor Sync: the proof stays on the thread
    // and in the scope of the authority guard that minted it.
    _critical_section: CriticalSection<'g>,
}

impl<'g> SandboxEffectGuard<'g> {
    /// The current fence the authority admitted: the asserted effect identity
    /// with its current (possibly renewed) expiry.
    #[must_use]
    pub fn fence(&self) -> &'g crate::SandboxEffectFence {
        self.current
    }

    /// Local lifetime remaining under the admitting authority.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.remaining
    }

    /// Require that `fence`, supplied separately to a fenced action, names the
    /// same effect this guard admitted. A physical action must not check one
    /// fence while holding admission for another.
    pub fn require_effect(
        &self,
        fence: &crate::SandboxEffectFence,
    ) -> Result<(), crate::SandboxError> {
        if self.current.same_effect_identity(fence) {
            Ok(())
        } else {
            Err(crate::SandboxError::fence_rejected(
                "fenced action names a different effect than its admitted guard",
            ))
        }
    }

    /// Require that a backend-recorded fence has not been superseded by a
    /// newer lease than this admitted effect. Providers call this inside their
    /// own compare-and-swap so the fence checked there is the fence admitted
    /// here, not a separately supplied value.
    pub fn require_not_superseded(
        &self,
        recorded: &crate::SandboxEffectFence,
    ) -> Result<(), crate::SandboxError> {
        use crate::FenceToken as _;
        if recorded.authorizes_successor(self.current) {
            Ok(())
        } else {
            Err(crate::SandboxError::fence_rejected(
                "backend-recorded Sandbox fence is newer than the admitted effect",
            ))
        }
    }
}

/// Run one synchronous Sandbox effect under its current operation-bound guard.
/// Shared by Memory acknowledgement/preparation and physical disposal. Durable
/// expiry stays in Control coordinates; this edge consumes the current local
/// proof. Re-enter after asynchronous waits: success cannot authorize an
/// in-flight remote commit or a later effect. Faulty ports cannot omit/repeat
/// the callback, widen its identity, or turn an after-effect error into success.
///
/// `apply` receives the [`SandboxEffectGuard`] minted inside the port's
/// critical section; fenced physical actions require it as a parameter.
pub fn with_current_sandbox_effect<T>(
    asserted: &crate::SandboxEffectFence,
    authority: &dyn CurrentExecutionAuthority,
    apply: impl FnOnce(&SandboxEffectGuard<'_>) -> Result<T, crate::SandboxError>,
) -> Result<T, crate::SandboxError> {
    with_checked_effect(asserted, authority, apply)
}

/// The same-effect admission rule: the port's current fence must be the
/// asserted effect (operation, owner, incarnation and epoch identical), must
/// not regress the asserted expiry, and must leave a positive local budget.
/// This is deliberately stricter than [`crate::FenceToken::authorizes_successor`]:
/// a successor lease is not the admitted effect.
fn admits_current_effect(
    asserted: &crate::SandboxEffectFence,
    current: &crate::SandboxEffectFence,
    remaining: Duration,
) -> bool {
    !remaining.is_zero()
        && asserted.same_effect_identity(current)
        && current.expires_at_unix_ms >= asserted.expires_at_unix_ms
}

fn with_checked_effect<T, A: CurrentExecutionAuthority + ?Sized>(
    asserted: &crate::SandboxEffectFence,
    authority: &A,
    apply: impl FnOnce(&SandboxEffectGuard<'_>) -> Result<T, crate::SandboxError>,
) -> Result<T, crate::SandboxError> {
    asserted.validate_identity()?;
    let mut apply = Some(apply);
    let mut result = None;
    let mut effect_error = None;
    let admitted = authority.with_current_effect(asserted, &mut |current, remaining| {
        if !admits_current_effect(asserted, current, remaining) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let apply = apply.take().ok_or(io::ErrorKind::PermissionDenied)?;
        let guard = SandboxEffectGuard {
            current,
            remaining,
            _critical_section: PhantomData,
        };
        match apply(&guard) {
            Ok(value) => {
                result = Some(value);
                Ok(())
            }
            Err(error) => {
                effect_error = Some(error);
                Err(io::Error::other(
                    "Sandbox effect failed under its current authority",
                ))
            }
        }
    });
    // The effect's own failure keeps its class; the port cannot relabel it.
    if let Some(error) = effect_error {
        return Err(error);
    }
    admitted.map_err(|error| {
        if error.kind() == io::ErrorKind::PermissionDenied {
            crate::SandboxError::fence_rejected(error.to_string())
        } else {
            crate::SandboxError::transport(format!("current execution authority: {error}"))
        }
    })?;
    result
        .ok_or_else(|| crate::SandboxError::fence_rejected("Sandbox authority omitted the effect"))
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
            let result = with_current_sandbox_effect(&initial, &port, |_| {
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

    #[test]
    fn effect_guard_is_minted_only_for_the_exact_current_effect() {
        // Guard decision table. Causes: C1 the port's current fence has the
        // asserted identity / a foreign operation / is an authorized successor
        // lease (higher epoch, or a renewal of another operation on the same
        // lease); C2 current expiry is renewed/equal/regressed; C3 the local
        // budget is positive/zero. Effects: E1 exactly one guard is minted and
        // it carries the port's current fence; E2 no guard, no callback,
        // FenceRejected. The guard rule is stricter than FenceToken succession:
        // a successor that `authorizes_successor` accepts is still NOT the
        // admitted effect (G5/G6), so an older effect can never act under the
        // lease its successor now holds.
        // | Rule | C1                    | C2        | C3   | Effect |
        // | G1   | same identity         | equal     | >0   | E1     |
        // | G2   | same identity         | renewed   | >0   | E1     |
        // | G3   | same identity         | regressed | >0   | E2     |
        // | G4   | same identity         | equal     | zero | E2     |
        // | G5   | higher-epoch successor| renewed   | >0   | E2     |
        // | G6   | same-lease other op   | renewed   | >0   | E2     |
        // | G7   | foreign lease         | renewed   | >0   | E2     |
        use crate::FenceToken as _;
        struct Port(crate::SandboxEffectFence, Duration);
        impl CurrentExecutionAuthority for Port {
            fn with_current(
                &self,
                _: &mut dyn FnMut(Duration) -> io::Result<()>,
            ) -> io::Result<()> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
            fn with_current_effect(
                &self,
                _: &crate::SandboxEffectFence,
                effect: &mut dyn FnMut(&crate::SandboxEffectFence, Duration) -> io::Result<()>,
            ) -> io::Result<()> {
                effect(&self.0, self.1)
            }
        }
        let asserted = crate::SandboxEffectFence::new("op", "owner", "boot", 4, 100).unwrap();
        let fence = |operation: &str, owner: &str, epoch, expiry| {
            crate::SandboxEffectFence::new(operation, owner, "boot", epoch, expiry).unwrap()
        };
        let live = Duration::from_secs(1);
        for (rule, current, budget, minted, successor) in [
            ("G1", fence("op", "owner", 4, 100), live, true, true),
            ("G2", fence("op", "owner", 4, 200), live, true, true),
            ("G3", fence("op", "owner", 4, 99), live, false, false),
            (
                "G4",
                fence("op", "owner", 4, 100),
                Duration::ZERO,
                false,
                true,
            ),
            ("G5", fence("op", "other", 5, 200), live, false, true),
            ("G6", fence("next", "owner", 4, 200), live, false, true),
            ("G7", fence("op", "other", 4, 200), live, false, false),
        ] {
            assert_eq!(
                asserted.authorizes_successor(&current),
                successor,
                "{rule} successor law"
            );
            let port = Port(current.clone(), budget);
            let mut observed = None;
            let result = with_current_sandbox_effect(&asserted, &port, |guard| {
                observed = Some((guard.fence().clone(), guard.remaining()));
                Ok(())
            });
            assert_eq!(result.is_ok(), minted, "{rule}");
            if minted {
                assert_eq!(observed, Some((current.clone(), budget)), "{rule} E1");
                // The admitted fence supersedes nothing it was checked against,
                // and a newer backend record is rejected under the guard.
                let newer = fence("op", "owner", 5, 1);
                let checks = with_current_sandbox_effect(&asserted, &port, |guard| {
                    Ok((
                        guard.require_not_superseded(&asserted).is_ok(),
                        guard.require_not_superseded(&newer).is_err(),
                        guard.require_effect(&asserted).is_ok(),
                        guard
                            .require_effect(&fence("next", "owner", 4, 100))
                            .is_err(),
                    ))
                })
                .unwrap();
                assert_eq!(checks, (true, true, true, true), "{rule} backend record");
            } else {
                assert!(observed.is_none(), "{rule} E2 no callback");
                assert!(
                    matches!(result, Err(crate::SandboxError::FenceRejected(_))),
                    "{rule} E2 class"
                );
            }
        }
    }
}
