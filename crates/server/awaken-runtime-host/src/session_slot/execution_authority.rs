// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Physical effect authorization is a view of the existing Worker and Session
//! owners, never another lease or lifecycle. Lock order: Worker -> slots -> GO.

use super::*;
#[cfg(target_os = "linux")]
use crate::session_environment::SessionEnvironment;
use awaken_provisioning_contract::{CurrentExecutionAuthority, SandboxEffectFence};
use awaken_session_resource_contract::SessionRealizationLease;
use std::{io, sync::Weak, time::Duration};

struct SessionExecutionAuthority {
    worker: Option<Arc<dyn CurrentExecutionAuthority>>,
    slots: Weak<Mutex<HashMap<String, SessionRuntimeSlot>>>,
    session: String,
    realization: Arc<tokio::sync::Mutex<()>>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    asserted: SessionRealizationLease,
    target: SessionEffectTarget,
}

/// The same authority guards two different targets. A physical execution
/// additionally needs an exact enrolled Environment; a provider effect may
/// precede Environment creation and instead pins its immutable operation.
enum SessionEffectTarget {
    #[cfg(target_os = "linux")]
    Execution(Weak<SessionEnvironment>),
    Provider(SandboxEffectFence),
}

fn denied() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
}

impl CurrentExecutionAuthority for SessionExecutionAuthority {
    fn with_current(&self, effect: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
        self.with_current_session(&mut |_, remaining| effect(remaining))
    }

    fn with_current_effect(
        &self,
        asserted: &SandboxEffectFence,
        effect: &mut dyn FnMut(&SandboxEffectFence, Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        let initial = match &self.target {
            SessionEffectTarget::Provider(initial) => initial,
            #[cfg(target_os = "linux")]
            SessionEffectTarget::Execution(_) => return Err(denied()),
        };
        if !initial.same_effect_identity(asserted)
            || asserted.expires_at_unix_ms < initial.expires_at_unix_ms
        {
            return Err(denied());
        }
        let mut invoked = false;
        self.with_current_session(&mut |lease, remaining| {
            let current = lease
                .sandbox_effect_fence(initial.operation_id.clone())
                .map_err(io::Error::other)?;
            if invoked
                || !asserted.same_effect_identity(&current)
                || current.expires_at_unix_ms < asserted.expires_at_unix_ms
            {
                return Err(denied());
            }
            invoked = true;
            effect(&current, remaining)
        })?;
        if !invoked {
            return Err(denied());
        }
        Ok(())
    }
}

impl SessionExecutionAuthority {
    fn capture(
        slots: &SessionRuntimeSlots,
        session: &str,
        slot: &SessionRuntimeSlot,
        asserted: &SessionRealizationLease,
        worker: Option<&Arc<dyn CurrentExecutionAuthority>>,
        target: SessionEffectTarget,
    ) -> Self {
        Self {
            worker: worker.cloned(),
            slots: Arc::downgrade(&slots.0),
            session: session.into(),
            realization: slot.realization.clone(),
            lifecycle: slot.lifecycle.clone(),
            asserted: asserted.clone(),
            target,
        }
    }

    fn with_current_session(
        &self,
        effect: &mut dyn FnMut(&SessionRealizationLease, Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        match &self.worker {
            Some(worker) => worker
                .with_current(&mut |remaining| self.with_session_current(effect, Some(remaining))),
            None => self.with_session_current(effect, None),
        }
    }
    fn with_session_current(
        &self,
        effect: &mut dyn FnMut(&SessionRealizationLease, Duration) -> io::Result<()>,
        worker_remaining: Option<Duration>,
    ) -> io::Result<()> {
        if worker_remaining.is_some_and(|remaining| remaining.is_zero()) {
            return Err(denied());
        }
        let slots = self.slots.upgrade().ok_or_else(denied)?;
        let slots = slots.lock().map_err(|_| denied())?;
        let slot = slots.get(&self.session).ok_or_else(denied)?;
        if !Arc::ptr_eq(&self.realization, &slot.realization)
            || !Arc::ptr_eq(&self.lifecycle, &slot.lifecycle)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Session realization slot was replaced",
            ));
        }
        match &self.target {
            #[cfg(target_os = "linux")]
            SessionEffectTarget::Execution(expected) => {
                let environment = expected.upgrade().ok_or_else(denied)?;
                let owned = slot
                    .environment_owner
                    .execution_environment()
                    .ok_or_else(denied)?;
                if !Arc::ptr_eq(&environment, owned) {
                    return Err(denied());
                }
            }
            SessionEffectTarget::Provider(_) => {}
        }
        let Some(session_remaining) = slot.realization_authority.remaining_for(&self.asserted)
        else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Session realization proof is no longer live",
            ));
        };
        let remaining =
            worker_remaining.map_or(session_remaining, |worker| worker.min(session_remaining));
        // Both original guards remain held through the synchronous effect.
        // Provider conversion anchors BOOTTIME before this callback, so a
        // wait for either mutex cannot mint additional physical lifetime.
        effect(
            slot.realization_authority.lease().ok_or_else(denied)?,
            remaining,
        )
    }
}

impl SessionRuntimeSlots {
    /// Capture one pre-Environment target through the existing authority
    /// implementation. The returned source and synchronous effect port share
    /// the same weak slot, lease, Worker proof and held-lock decision.
    pub(crate) fn provider_effect_authority(
        &self,
        session: &str,
        initial: &SandboxEffectFence,
        worker: Option<&Arc<dyn CurrentExecutionAuthority>>,
    ) -> Result<impl CurrentExecutionAuthority + use<>, crate::HostError> {
        initial
            .validate_identity()
            .map_err(|error| crate::HostError::internal(error.to_string()))?;
        self.read(session, |slot| {
            let lease = slot.realization_authority.lease().ok_or_else(|| {
                crate::HostError::internal("Session realization proof is no longer live")
            })?;
            let current = lease
                .sandbox_effect_fence(initial.operation_id.clone())
                .map_err(|error| crate::HostError::internal(error.to_string()))?;
            if !initial.same_effect_identity(&current)
                || current.expires_at_unix_ms < initial.expires_at_unix_ms
            {
                return Err(crate::HostError::internal(
                    "Session realization generation changed",
                ));
            }
            Ok(SessionExecutionAuthority::capture(
                self,
                session,
                slot,
                lease,
                worker,
                SessionEffectTarget::Provider(initial.clone()),
            ))
        })
        .ok_or_else(|| crate::HostError::internal("Session realization slot is absent"))?
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn execution_environments(&self) -> Vec<Arc<SessionEnvironment>> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .values()
            .filter_map(|slot| slot.environment_owner.execution_environment())
            .filter(|environment| {
                matches!(environment.as_ref(),
                SessionEnvironment::Workdir { sandbox, .. } if sandbox.has_scoped_execution())
            })
            .cloned()
            .collect()
    }

    /// Called while the caller owns this map's existing mutex and has enrolled
    /// the exact candidate. Terminal-only owners with no live proof stay
    /// selected-but-unbound; they may clean up, but can never start work.
    #[cfg(target_os = "linux")]
    pub(crate) fn bind_execution_authority(
        &self,
        session: &str,
        slot: &SessionRuntimeSlot,
        environment: &Arc<SessionEnvironment>,
        worker: Option<&Arc<dyn CurrentExecutionAuthority>>,
    ) -> Result<(), crate::HostError> {
        let SessionEnvironment::Workdir { sandbox, .. } = environment.as_ref() else {
            return Ok(());
        };
        if !sandbox.needs_execution_authority() {
            return Ok(());
        }
        let Some(asserted) = slot.realization_authority.lease() else {
            return Ok(());
        };
        sandbox
            .bind_execution_authority(Arc::new(SessionExecutionAuthority::capture(
                self,
                session,
                slot,
                asserted,
                worker,
                SessionEffectTarget::Execution(Arc::downgrade(environment)),
            )))
            .map_err(|error| crate::HostError::internal(error.to_string()))?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    struct Worker(Mutex<Option<Duration>>);
    impl CurrentExecutionAuthority for Worker {
        fn with_current(
            &self,
            effect: &mut dyn FnMut(Duration) -> io::Result<()>,
        ) -> io::Result<()> {
            let proof = self.0.lock().map_err(|_| denied())?;
            effect(proof.ok_or_else(denied)?)
        }
    }

    struct Fixture {
        slots: SessionRuntimeSlots,
        environment: Arc<SessionEnvironment>,
        worker: Arc<Worker>,
        authority: Arc<SessionExecutionAuthority>,
        _base: tempfile::TempDir,
    }

    impl Fixture {
        async fn new() -> Self {
            let base = tempfile::tempdir().unwrap();
            let sandbox = awaken_sandbox_local::LocalProvider::new(base.path())
                .create_sandbox(&crate::session_environment::environment_test_spec())
                .await
                .unwrap();
            let environment = Arc::new(SessionEnvironment::workdir(sandbox));
            let slots = SessionRuntimeSlots::default();
            let worker = Arc::new(Worker(Mutex::new(Some(Duration::from_secs(30)))));
            let asserted = SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
                    + 60_000,
            };
            let authority = slots.update("session", |slot| {
                slot.realization_authority = SessionRealizationAuthority::Live(
                    crate::session_slot::test_observed_lease(asserted.clone()),
                );
                slot.environment_owner = Self::candidate(environment.clone());
                Arc::new(SessionExecutionAuthority {
                    worker: Some(worker.clone()),
                    slots: Arc::downgrade(&slots.0),
                    session: "session".into(),
                    realization: slot.realization.clone(),
                    lifecycle: slot.lifecycle.clone(),
                    asserted,
                    target: SessionEffectTarget::Execution(Arc::downgrade(&environment)),
                })
            });
            Self {
                slots,
                environment,
                worker,
                authority,
                _base: base,
            }
        }

        fn candidate(environment: Arc<SessionEnvironment>) -> SessionEnvironmentOwner {
            SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(
                UnboundSessionEnvironment {
                    binding: serde_json::to_string(&environment.handle()).unwrap(),
                    environment,
                    origin: UnboundSessionEnvironmentOrigin::New,
                },
            ))
        }

        fn current(&self) -> io::Result<Duration> {
            let mut remaining = None;
            self.authority.with_current(&mut |duration| {
                assert!(remaining.is_none());
                remaining = Some(duration);
                Ok(())
            })?;
            remaining.ok_or_else(|| io::Error::other("omitted effect"))
        }
    }

    #[tokio::test]
    async fn execution_authority_intersects_current_owners_and_rejects_every_stale_coordinate() {
        // Cause/effect graph: C1 current positive Worker proof; C2 live Session
        // lease; C3 exact owner/incarnation/epoch + nondecreasing expiry; C4 same
        // slot allocation; C5 same active Environment Arc. E1 one guarded effect
        // with min(current budgets); E2 denial with zero effects. Constraint:
        // all C are ANDed, so independent false rows provide MC/DC coverage.
        // S1 all true -> E1; S2-S15 falsify one coordinate -> E2. S16 extends
        // Session expiry and S17 refreshes Worker proof -> E1, no frozen TTL.
        // S29 Resident -> E1; S30 Retiring -> E2; S31 shorter Session budget ->
        // E1 capped by Session, not Worker. Expiry row keeps generation/expiry
        // comparison equal so it independently exercises the time predicate.
        for rule in [
            "ok",
            "worker_closed",
            "worker_zero",
            "uninstalled",
            "closed",
            "expired",
            "owner",
            "incarnation",
            "epoch",
            "expiry_regression",
            "removed",
            "slot_aba",
            "environment_aba",
            "vacant",
            "pending",
            "renew_session",
            "renew_worker",
            "resident",
            "retiring",
            "short_session",
        ] {
            let mut f = Fixture::new().await;
            if matches!(rule, "expired" | "short_session") {
                Arc::get_mut(&mut f.authority)
                    .unwrap()
                    .asserted
                    .expires_at_unix_ms = if rule == "expired" {
                    0
                } else {
                    crate::terminal_repository_publication::runtime_unix_now_ms() + 5_000
                };
            }
            match rule {
                "worker_closed" => *f.worker.0.lock().unwrap() = None,
                "worker_zero" => *f.worker.0.lock().unwrap() = Some(Duration::ZERO),
                "renew_worker" => *f.worker.0.lock().unwrap() = Some(Duration::from_secs(5)),
                "removed" => {
                    f.slots.remove("session");
                }
                "slot_aba" => {
                    let old = f.slots.remove("session").unwrap();
                    f.slots.update("session", |slot| {
                        slot.realization_authority = old.realization_authority;
                        slot.environment_owner = old.environment_owner;
                    });
                }
                _ => f.slots.update("session", |slot| {
                    let mut lease = f.authority.asserted.clone();
                    match rule {
                        "uninstalled" => {
                            slot.realization_authority = SessionRealizationAuthority::Uninstalled;
                            return;
                        }
                        "closed" => {
                            slot.close_realization_authority(None);
                            return;
                        }
                        "expired" => lease.expires_at_unix_ms = 0,
                        "owner" => lease.owner = "other".into(),
                        "incarnation" => lease.runtime_incarnation = "other".into(),
                        "epoch" => lease.epoch += 1,
                        "expiry_regression" => lease.expires_at_unix_ms -= 1,
                        "renew_session" => lease.expires_at_unix_ms += 10_000,
                        "resident" => {
                            let candidate = slot
                                .environment_owner
                                .unpublished_candidate()
                                .unwrap()
                                .clone();
                            slot.environment_owner =
                                SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
                                    identity: BoundSessionEnvironmentIdentity::LegacyDirect(
                                        LegacyDirectEnvironmentProvenance::DurableBinding {
                                            binding: candidate.binding.clone(),
                                            effect: LegacyEnvironmentEffect::Absent,
                                        },
                                    ),
                                    binding: candidate.binding,
                                    environment: candidate.environment,
                                });
                        }
                        "retiring" => {
                            let candidate = slot
                                .environment_owner
                                .unpublished_candidate()
                                .unwrap()
                                .clone();
                            slot.environment_owner =
                                SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                                    cause: SessionEnvironmentRetirementCause::UnpublishedCandidate,
                                    owned: RetiringEnvironmentOwner::Unbound(candidate),
                                });
                        }
                        "environment_aba" => {
                            let SessionEnvironment::Workdir { sandbox, .. } =
                                f.environment.as_ref()
                            else {
                                unreachable!()
                            };
                            slot.environment_owner =
                                Fixture::candidate(Arc::new(SessionEnvironment::Workdir {
                                    sandbox: sandbox.clone(),
                                    hand: None,
                                }));
                        }
                        "vacant" => slot.environment_owner = SessionEnvironmentOwner::Vacant,
                        "pending" => {
                            slot.environment_owner = SessionEnvironmentOwner::Preparing(
                                SessionEnvironmentPreparation::AwaitingAdoption {
                                    identity: BoundSessionEnvironmentIdentity::LegacyDirect(
                                        LegacyDirectEnvironmentProvenance::DurableBinding {
                                            binding: "unused".into(),
                                            effect: LegacyEnvironmentEffect::Absent,
                                        },
                                    ),
                                    binding: "unused".into(),
                                },
                            )
                        }
                        "ok" | "short_session" => {}
                        _ => unreachable!(),
                    }
                    slot.realization_authority =
                        SessionRealizationAuthority::Live(if rule == "expired" {
                            crate::session_slot::test_expired_observed_lease(lease)
                        } else {
                            crate::session_slot::test_observed_lease(lease)
                        });
                }),
            }
            let mut calls = 0;
            let result = f.authority.with_current(&mut |remaining| {
                calls += 1;
                assert!(remaining > Duration::ZERO && remaining <= Duration::from_secs(30));
                if rule == "renew_worker" {
                    assert_eq!(remaining, Duration::from_secs(5));
                }
                if rule == "short_session" {
                    assert!(remaining <= Duration::from_secs(5));
                }
                Ok(())
            });
            let allowed = matches!(
                rule,
                "ok" | "renew_session" | "renew_worker" | "resident" | "short_session"
            );
            assert_eq!(result.is_ok(), allowed, "{rule}");
            assert_eq!(calls, usize::from(allowed), "{rule}");
        }
    }

    #[tokio::test]
    async fn direct_local_authority_requires_the_real_session_without_a_worker_receipt() {
        // L1 unregistered local Host has no Worker aggregate; live exact Session
        // plus exact Environment admits one bounded effect under the slot lock.
        // L2 closing that Session denies with zero further effects. Registered
        // Worker missing/zero/closed proof rows remain in S2/S3 above; absence
        // here is a topology fact, never a synthesized infinite root grant.
        let mut f = Fixture::new().await;
        Arc::get_mut(&mut f.authority).unwrap().worker = None;
        let mut calls = 0;
        f.authority
            .with_current(&mut |remaining| {
                calls += 1;
                assert!(remaining > Duration::ZERO && remaining <= Duration::from_secs(40));
                assert!(matches!(
                    f.slots.0.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 1, "L1");
        f.slots.close_realization("session", None);
        assert!(
            f.authority
                .with_current(&mut |_| {
                    calls += 1;
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(calls, 1, "L2");
    }

    #[tokio::test]
    async fn execution_authority_holds_original_guards_and_propagates_effect_failure_once() {
        // S18 callback under both real mutexes: neither Worker revoke nor slot
        // replacement can interleave between check and GO. S19 callback failure
        // is propagated once; S20 close after release denies the next callback.
        let f = Fixture::new().await;
        let mut calls = 0;
        let result = f.authority.with_current(&mut |_| {
            calls += 1;
            assert!(matches!(
                f.worker.0.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            assert!(matches!(
                f.slots.0.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            Err(io::Error::other("effect failed"))
        });
        assert_eq!(result.unwrap_err().to_string(), "effect failed");
        assert_eq!(calls, 1);
        f.slots.close_realization("session", None);
        assert!(f.current().is_err());
    }

    #[tokio::test]
    async fn provider_and_execution_targets_share_guards_without_weakening_environment_binding() {
        // Cause/effect decision table: G1 provider/execution target; G2 exact
        // Environment present/absent; G3 live/closed slot; G4 callback success/
        // failure. Live provider needs the captured operation/slot but not an
        // Environment; live execution additionally needs the exact Environment.
        // Both hold ORIGINAL Worker/slot guards through one synchronous callback.
        // G1 provider+failure returns the error once, without caching a grant;
        // G2 removing only Environment admits provider, denies execution;
        // G3 closing the slot denies both. An execution target cannot be used
        // as a Container renewal source, even while its own execution is live.
        let f = Fixture::new().await;
        let initial = f.authority.asserted.sandbox_effect_fence("create").unwrap();
        let worker: Arc<dyn CurrentExecutionAuthority> = f.worker.clone();
        let provider = f
            .slots
            .provider_effect_authority("session", &initial, Some(&worker))
            .unwrap();
        assert!(f.authority.current_effect_fence(&initial).is_err());
        let mut calls = 0;
        let failure = provider.with_current_effect(&initial, &mut |current, remaining| {
            assert_eq!(current, &initial);
            calls += 1;
            assert_eq!(remaining, Duration::from_secs(30));
            assert!(matches!(
                f.worker.0.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            assert!(matches!(
                f.slots.0.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            Err(io::Error::other("provider effect failed"))
        });
        assert_eq!(failure.unwrap_err().to_string(), "provider effect failed");
        assert_eq!(calls, 1);
        f.slots.update("session", |slot| {
            slot.environment_owner = SessionEnvironmentOwner::Vacant
        });
        assert!(f.current().is_err());
        assert_eq!(provider.current_effect_fence(&initial).unwrap(), initial);
        provider
            .with_current_effect(&initial, &mut |_, _| {
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 2);
        f.slots.close_realization("session", None);
        assert!(
            provider
                .with_current(&mut |_| panic!("closed provider"))
                .is_err()
        );
        assert!(provider.current_effect_fence(&initial).is_err());
        assert!(f.current().is_err());
    }

    #[tokio::test]
    async fn execution_authority_poison_and_callback_unwind_fail_closed() {
        // S32 slot poison alone; S33 callback unwind poisons both held owners.
        // Neither uncertain state can grant a later effect or reopen a scope.
        for callback in [false, true] {
            let f = Fixture::new().await;
            let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if callback {
                    let _ = f.authority.with_current(&mut |_| panic!("callback failed"));
                } else {
                    let _slot = f.slots.0.lock().unwrap();
                    panic!("slot writer failed");
                }
            }));
            assert!(unwind.is_err());
            assert!(f.current().is_err());
        }
    }

    #[tokio::test]
    async fn execution_authority_does_not_retain_environment_or_slot_owner() {
        // S21: outstanding authority after Host/Environment drop cannot keep
        // the ownership graph alive or authorize work (Weak edges, no cycle).
        let f = Fixture::new().await;
        let environment = Arc::downgrade(&f.environment);
        let slots = Arc::downgrade(&f.slots.0);
        let authority = f.authority.clone();
        drop(f);
        assert!(slots.upgrade().is_none());
        assert!(environment.upgrade().is_none());
        assert!(
            authority
                .with_current(&mut |_| panic!("dead owner"))
                .is_err()
        );
    }
}
