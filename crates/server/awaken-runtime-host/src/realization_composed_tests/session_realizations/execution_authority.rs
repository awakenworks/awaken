//! Original Host composition tests over canonical realization code.

use crate::session_environment::SessionEnvironment;
use awaken_provisioning_contract::CurrentExecutionAuthority;
use awaken_session_configuration_contract::resource::SessionRealizationLease;
use std::sync::Arc;
use std::{io, time::Duration};

use awaken_session_realization::execution_authority::*;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::environment_owner::*;
    use crate::installed_session::InstalledSessions;
    use crate::realization_state::SessionRealizationAuthority;
    use awaken_session_environment::SandboxEnvironment;
    use awaken_session_environment::test_support::SandboxEnvironmentAccess as _;
    use std::sync::Mutex;

    struct Worker(Mutex<Option<Duration>>);
    impl CurrentExecutionAuthority for Worker {
        fn with_current(
            &self,
            effect: &mut dyn FnMut(Duration) -> io::Result<()>,
        ) -> io::Result<()> {
            let proof = self
                .0
                .lock()
                .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
            effect(proof.ok_or_else(|| io::Error::from(io::ErrorKind::PermissionDenied))?)
        }
    }

    struct Fixture {
        slots: Arc<InstalledSessions>,
        environment: Arc<SessionEnvironment>,
        worker: Arc<Worker>,
        authority: Arc<SessionExecutionAuthority>,
        _base: tempfile::TempDir,
    }

    impl Fixture {
        async fn new() -> Self {
            let base = tempfile::tempdir().unwrap();
            let sandbox = awaken_sandbox_local::LocalProvider::new(base.path())
                .create_sandbox(&awaken_session_environment::test_support::spec())
                .await
                .unwrap();
            let environment =
                Arc::new(SandboxEnvironment::workdir(sandbox).into_session_environment());
            let slots = Arc::new(InstalledSessions::default());
            let worker = Arc::new(Worker(Mutex::new(Some(Duration::from_secs(30)))));
            let asserted = SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: awaken_session_realization::test_support::runtime_unix_now_ms()
                    + 60_000,
            };
            slots.install("session");
            let authority = slots
                .realizations()
                .update_realization("session", |r| {
                    r.execution.authority = SessionRealizationAuthority::Live(
                        awaken_session_realization::test_support::test_observed_lease(
                            asserted.clone(),
                        ),
                    );
                    r.environment.owner = Self::candidate(environment.clone());
                    Arc::new(SessionExecutionAuthority::for_test(
                        &ExecutionAuthority::new(slots.realizations(), Some(worker.clone())),
                        "session",
                        r,
                        asserted,
                        &environment,
                    ))
                })
                .expect("installed Sessions");
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
                    intent: None,
                    binding: serde_json::to_string(&environment.lifecycle().handle()).unwrap(),
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
                    .asserted_for_test_mut()
                    .expires_at_unix_ms = if rule == "expired" {
                    0
                } else {
                    awaken_session_realization::test_support::runtime_unix_now_ms() + 5_000
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
                    f.slots.install_realization("session", |r| {
                        r.execution.authority = old.realization.execution.authority;
                        r.environment.owner = old.realization.environment.owner;
                    });
                }
                _ => f.slots.realizations().update_realization("session", |r| {
                    let mut lease = f.authority.asserted_for_test().clone();
                    match rule {
                        "uninstalled" => {
                            r.execution.authority = SessionRealizationAuthority::Uninstalled;
                            return;
                        }
                        "closed" => {
                            r.close_authority(None);
                            return;
                        }
                        "expired" => lease.expires_at_unix_ms = 0,
                        "owner" => lease.owner = "other".into(),
                        "incarnation" => lease.runtime_incarnation = "other".into(),
                        "epoch" => lease.epoch += 1,
                        "expiry_regression" => lease.expires_at_unix_ms -= 1,
                        "renew_session" => lease.expires_at_unix_ms += 10_000,
                        "resident" => {
                            let candidate =
                                r.environment.owner.unpublished_candidate().unwrap().clone();
                            r.environment.owner =
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
                            let candidate =
                                r.environment.owner.unpublished_candidate().unwrap().clone();
                            r.environment.owner =
                                SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                                    cause: SessionEnvironmentRetirementCause::UnpublishedCandidate,
                                    owned: RetiringEnvironmentOwner::Unbound(candidate),
                                });
                        }
                        "environment_aba" => {
                            let SandboxEnvironment::Workdir { sandbox, .. } =
                                f.environment.sandbox_environment()
                            else {
                                unreachable!()
                            };
                            r.environment.owner = Fixture::candidate(Arc::new(
                                SandboxEnvironment::Workdir {
                                    sandbox: sandbox.clone(),
                                    hand: None,
                                }
                                .into_session_environment(),
                            ));
                        }
                        "vacant" => r.environment.owner = SessionEnvironmentOwner::Vacant,
                        "pending" => {
                            r.environment.owner = SessionEnvironmentOwner::Preparing(
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
                    r.execution.authority =
                        SessionRealizationAuthority::Live(if rule == "expired" {
                            awaken_session_realization::test_support::test_expired_observed_lease(
                                lease,
                            )
                        } else {
                            awaken_session_realization::test_support::test_observed_lease(lease)
                        });
                }).expect("installed Sessions"),
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
        *Arc::get_mut(&mut f.authority)
            .unwrap()
            .worker_for_test_mut() = None;
        let mut calls = 0;
        f.authority
            .with_current(&mut |remaining| {
                calls += 1;
                assert!(remaining > Duration::ZERO && remaining <= Duration::from_secs(40));
                assert!(matches!(
                    f.slots.test_map().try_lock(),
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
                f.slots.test_map().try_lock(),
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
        let initial = f
            .authority
            .asserted_for_test()
            .sandbox_effect_fence("create")
            .unwrap();
        let worker: Arc<dyn CurrentExecutionAuthority> = f.worker.clone();
        let sessions = f.slots.realizations();
        let provider = ExecutionAuthority::new(sessions, Some(worker))
            .provider_effect_authority("session", &initial)
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
                f.slots.test_map().try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            Err(io::Error::other("provider effect failed"))
        });
        assert_eq!(failure.unwrap_err().to_string(), "provider effect failed");
        assert_eq!(calls, 1);
        f.slots
            .realizations()
            .update_realization("session", |r| {
                r.environment.owner = SessionEnvironmentOwner::Vacant
            })
            .expect("installed Sessions");
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
        // The original local registry now implements Source itself. Every
        // view borrows that same Mutex through the same Source Arc: healthy
        // absence remains absence, while poison remains Internal/zero effects.
        // S32/S33 below retain both fresh-capture and held-callback failures.
        // Cause/effect table: C1 installed/missing slot; C2 healthy/poisoned
        // slot lock; C3 poison from a slot writer (S32) or the held effect
        // callback (S33, also poisons Worker). E1 healthy exact capture succeeds;
        // E2 healthy missing capture reports absence without creating a slot;
        // E3 poison before fresh capture returns Internal, never absence/unwind;
        // E4 poison after capture denies the effect with zero callbacks.
        // Both S32/S33 cover E1-E4. The initiating C3 panic is intentional;
        // only later capture/effect behavior must not unwind or reopen a scope.
        for callback in [false, true] {
            let f = Fixture::new().await;
            let initial = f
                .authority
                .asserted_for_test()
                .sandbox_effect_fence("create")
                .unwrap();
            let execution = ExecutionAuthority::new(f.slots.realizations(), Some(f.worker.clone()));
            let provider = execution
                .provider_effect_authority("session", &initial)
                .expect("E1 healthy exact capture");
            let absent = execution
                .provider_effect_authority("missing", &initial)
                .err()
                .expect("E2 genuinely absent slot");
            assert!(
                matches!(
                    absent,
                    crate::realization_error::RealizationError::AuthorityLost {
                        cause: crate::realization_error::AuthorityLoss::SessionAbsent,
                        ..
                    }
                ),
                "E2 absence is a typed authority loss: {absent}"
            );
            assert!(!f.slots.contains("missing"), "E2 no slot creation");
            let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if callback {
                    let _ = f.authority.with_current(&mut |_| panic!("callback failed"));
                } else {
                    let _slot = f.slots.test_map().lock().unwrap();
                    panic!("slot writer failed");
                }
            }));
            assert!(unwind.is_err());
            assert!(f.current().is_err());
            let mut calls = 0;
            assert!(
                provider
                    .with_current_effect(&initial, &mut |_, _| {
                        calls += 1;
                        Ok(())
                    })
                    .is_err(),
                "E4 captured provider denies after poison"
            );
            assert_eq!(calls, 0, "E4 no effect callback");
            let fresh = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                execution.provider_effect_authority("session", &initial)
            }))
            .expect("E3 fresh capture must not unwind");
            let error = fresh.err().expect("E3 poisoned capture must reject");
            assert!(matches!(
                error,
                crate::realization_error::RealizationError::Poisoned(
                    crate::realization_error::RealizationLock::InstalledSessions
                )
            ));
            assert_eq!(
                error.disposition(),
                crate::realization_error::Disposition::Retry,
                "E3 poison denies this process's effect; a healthy owner may retry"
            );
            assert_eq!(error.code(), crate::realization_error::POISONED_CODE);
            assert_ne!(
                error.to_string(),
                absent.to_string(),
                "E3 poison is not absence"
            );
        }
    }

    #[tokio::test]
    async fn execution_authority_does_not_retain_environment_or_slot_owner() {
        // C1 a temporary view captures a Weak Source while the registry lives;
        // C2 all strong registry/Environment owners are dropped. The same Arc
        // registry backs every view, so C1 cannot expire early, and C2 must
        // still expire without an effect or a self-cycle. The assertions below
        // now observe that original owner rather than an inner Mutex Arc.
        // S21: outstanding authority after Host/Environment drop cannot keep
        // the ownership graph alive or authorize work (Weak edges, no cycle).
        let f = Fixture::new().await;
        let environment = Arc::downgrade(&f.environment);
        let slots = Arc::downgrade(&f.slots);
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
