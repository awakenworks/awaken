// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Physical effect authorization is a view of the existing Worker and Session
//! owners, never another lease or lifecycle. Lock order: Worker -> installed
//! Sessions -> GO. A poisoned lock denies the effect.

use std::sync::{Arc, Weak};
use std::{io, time::Duration};

use awaken_provisioning_contract::{CurrentExecutionAuthority, SandboxEffectFence};
use awaken_session_configuration_contract::resource::SessionRealizationLease;

use super::{SessionRealization, SessionRealizationSource, SessionRealizations};
use crate::host_error::HostError;
#[cfg(target_os = "linux")]
use crate::session_environment::SessionEnvironment;

/// The authority every physical Environment effect of the realization executor
/// runs under: the Worker's registry proof (absent on a direct Host) intersected
/// with the Session's live realization lease.
#[derive(Clone)]
pub(crate) struct ExecutionAuthority {
    pub(crate) sessions: SessionRealizations,
    /// Worker-owned registry proof. Direct Hosts have no Worker registration;
    /// every registered Worker installs this before enabling dispatch.
    pub(crate) worker: Option<Arc<dyn CurrentExecutionAuthority>>,
}

struct SessionExecutionAuthority {
    worker: Option<Arc<dyn CurrentExecutionAuthority>>,
    sessions: Weak<dyn SessionRealizationSource>,
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

    // Linux has an Execution target that this effect must reject; other hosts
    // only have Provider. Keep one match for the shared authorization path.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(clippy::infallible_destructuring_match)
    )]
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
            || asserted.expires_at_unix_ms() < initial.expires_at_unix_ms()
        {
            return Err(denied());
        }
        let mut invoked = false;
        self.with_current_session(&mut |lease, remaining| {
            let current = lease
                .sandbox_effect_fence(initial.operation_id())
                .map_err(io::Error::other)?;
            if invoked
                || !asserted.same_effect_identity(&current)
                || current.expires_at_unix_ms() < asserted.expires_at_unix_ms()
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
        let sessions = self.sessions.upgrade().ok_or_else(denied)?;
        let mut outcome = Err(denied());
        sessions
            .visit(&self.session, false, &mut |access| {
                outcome = self.effect_under_lock(access.realization, effect, worker_remaining);
            })
            .map_err(|_| denied())?;
        outcome
    }

    fn effect_under_lock(
        &self,
        realization: &SessionRealization,
        effect: &mut dyn FnMut(&SessionRealizationLease, Duration) -> io::Result<()>,
        worker_remaining: Option<Duration>,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(&self.realization, &realization.execution.phase)
            || !Arc::ptr_eq(&self.lifecycle, &realization.environment.lifecycle)
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
                let owned = realization
                    .environment
                    .owner
                    .execution_environment()
                    .ok_or_else(denied)?;
                if !Arc::ptr_eq(&environment, owned) {
                    return Err(denied());
                }
            }
            SessionEffectTarget::Provider(_) => {}
        }
        let Some(session_remaining) = realization
            .execution
            .authority
            .remaining_for(&self.asserted)
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
            realization.execution.authority.lease().ok_or_else(denied)?,
            remaining,
        )
    }
}

impl ExecutionAuthority {
    /// Lock order: Worker proof -> Session slots -> synchronous provider grant.
    /// No async owner lock or external I/O may be held by this callback.
    pub(crate) fn authorize_worker_effect<T>(
        &self,
        effect: impl FnOnce() -> Result<T, HostError>,
    ) -> Result<T, HostError> {
        let Some(authority) = &self.worker else {
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
                HostError::internal(format!("Worker execution authority denied: {error}"))
            })?;
        result.ok_or_else(|| HostError::internal("Worker authority omitted the effect"))?
    }

    fn capture(
        &self,
        session: &str,
        realization: &SessionRealization,
        asserted: &SessionRealizationLease,
        target: SessionEffectTarget,
    ) -> SessionExecutionAuthority {
        SessionExecutionAuthority {
            worker: self.worker.clone(),
            sessions: Arc::downgrade(&self.sessions.0),
            session: session.into(),
            realization: realization.execution.phase.clone(),
            lifecycle: realization.environment.lifecycle.clone(),
            asserted: asserted.clone(),
            target,
        }
    }

    /// Capture one pre-Environment target through the existing authority
    /// implementation. The returned source and synchronous effect port share
    /// the same weak Session view, lease, Worker proof and held-lock decision.
    pub(crate) fn provider_effect_authority(
        &self,
        session: &str,
        initial: &SandboxEffectFence,
    ) -> Result<impl CurrentExecutionAuthority + use<>, HostError> {
        let mut capture = Some(|realization: &SessionRealization| {
            let lease = realization.execution.authority.lease().ok_or_else(|| {
                HostError::internal("Session realization proof is no longer live")
            })?;
            let current = lease
                .sandbox_effect_fence(initial.operation_id())
                .map_err(|error| HostError::internal(error.to_string()))?;
            if !initial.same_effect_identity(&current)
                || current.expires_at_unix_ms() < initial.expires_at_unix_ms()
            {
                return Err(HostError::internal(
                    "Session realization generation changed",
                ));
            }
            Ok(self.capture(
                session,
                realization,
                lease,
                SessionEffectTarget::Provider(initial.clone()),
            ))
        });
        let mut outcome = None;
        self.sessions
            .0
            .visit(session, false, &mut |access| {
                outcome = capture.take().map(|capture| capture(access.realization));
            })
            .map_err(|_| HostError::internal("installed sessions mutex poisoned"))?;
        outcome.ok_or_else(|| HostError::internal("Session realization slot is absent"))?
    }

    /// Called while the caller holds the installed Sessions' lock and has
    /// enrolled the exact candidate. Terminal-only owners with no live proof stay
    /// selected-but-unbound; they may clean up, but can never start work.
    #[cfg(target_os = "linux")]
    pub(crate) fn bind(
        &self,
        session: &str,
        realization: &SessionRealization,
        environment: &Arc<SessionEnvironment>,
    ) -> Result<(), HostError> {
        let Some(scoped) = environment.scoped_execution() else {
            return Ok(());
        };
        if !scoped.needs_execution_authority() {
            return Ok(());
        }
        let Some(asserted) = realization.execution.authority.lease() else {
            return Ok(());
        };
        scoped
            .bind_execution_authority(Arc::new(self.capture(
                session,
                realization,
                asserted,
                SessionEffectTarget::Execution(Arc::downgrade(environment)),
            )))
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::environment_owner::*;
    use crate::installed_session::InstalledSessions;
    use crate::session_environment::{SandboxEnvironment, SandboxEnvironmentAccess as _};
    use crate::session_realizations::SessionRealizationAuthority;
    use std::sync::Mutex;

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
                .create_sandbox(&crate::session_environment::environment_test_spec())
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
                expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
                    + 60_000,
            };
            let authority = slots.realizations().update_realization("session", |r| {
                r.execution.authority = SessionRealizationAuthority::Live(
                    crate::installed_session::test_observed_lease(asserted.clone()),
                );
                r.environment.owner = Self::candidate(environment.clone());
                Arc::new(SessionExecutionAuthority {
                    worker: Some(worker.clone()),
                    sessions: Arc::downgrade(&slots.realizations().0),
                    session: "session".into(),
                    realization: r.execution.phase.clone(),
                    lifecycle: r.environment.lifecycle.clone(),
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
                    f.slots.realizations().update_realization("session", |r| {
                        r.execution.authority = old.realization.execution.authority;
                        r.environment.owner = old.realization.environment.owner;
                    });
                }
                _ => f.slots.realizations().update_realization("session", |r| {
                    let mut lease = f.authority.asserted.clone();
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
                            crate::installed_session::test_expired_observed_lease(lease)
                        } else {
                            crate::installed_session::test_observed_lease(lease)
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
        let initial = f.authority.asserted.sandbox_effect_fence("create").unwrap();
        let worker: Arc<dyn CurrentExecutionAuthority> = f.worker.clone();
        let sessions = f.slots.realizations();
        let provider = ExecutionAuthority {
            sessions,
            worker: Some(worker),
        }
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
        f.slots.realizations().update_realization("session", |r| {
            r.environment.owner = SessionEnvironmentOwner::Vacant
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
            let initial = f.authority.asserted.sandbox_effect_fence("create").unwrap();
            let execution = ExecutionAuthority {
                sessions: f.slots.realizations(),
                worker: Some(f.worker.clone()),
            };
            let provider = execution
                .provider_effect_authority("session", &initial)
                .expect("E1 healthy exact capture");
            let absent = execution
                .provider_effect_authority("missing", &initial)
                .err()
                .expect("E2 genuinely absent slot");
            assert_eq!(absent.message, "Session realization slot is absent");
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
            assert_eq!(error.kind, crate::host_error::HostErrorKind::Internal);
            assert_eq!(error.code, "internal");
            assert_eq!(error.message, "installed sessions mutex poisoned");
            assert_ne!(error.message, absent.message, "E3 poison is not absence");
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

/// Per-Session state held under the canonical installed-slot mutex.
#[derive(Default)]
pub(crate) struct ExecutionAuthorityState {
    /// Serializes the canonical realization phase driver for this Session.
    /// Lease renewal may advance authority while this lock is held, but it must
    /// not start a second Stage/Publish driver. This is deliberately separate
    /// from `lifecycle`, which is re-entered by Environment materialization.
    pub phase: Arc<tokio::sync::Mutex<()>>,
    /// Last accepted aggregate version and equality digest; rebuildable, never authority.
    pub(crate) frozen_receipt: Option<super::FrozenProjectionReceipt>,
    /// Current Control-issued projection authority. It is a live cache used to
    /// request renewal; durable ownership remains in the Session aggregate.
    pub authority: super::SessionRealizationAuthority,
    /// Wakes a claim resolver waiting for the Session application's initial
    /// restart reassignment. The lease above remains the only projected fact;
    /// this notification carries no authority or parallel state.
    pub changed: Arc<tokio::sync::Notify>,
}
