// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
use super::*;
use crate::realization_state::SessionRealizationAuthority;
use awaken_provisioning_contract::test_support::FenceTamper as _;
use awaken_provisioning_contract::{CurrentExecutionAuthority, SandboxEffectFence};
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_realization_contract::SessionRealizationLeaseReceipt;
use std::{io, sync::Mutex, time::Duration};

struct Worker(Mutex<Option<Duration>>);

impl CurrentExecutionAuthority for Worker {
    fn with_current(&self, effect: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
        let remaining = self.0.lock().unwrap();
        effect(remaining.ok_or(io::ErrorKind::PermissionDenied)?)
    }
}

async fn dependencies() -> crate::HostDependencies {
    crate::HostDependencies::ephemeral(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub")
        .await
}

async fn host() -> Arc<SharedHost> {
    (dependencies().await)
        .build()
        .expect("validated Host dependencies")
}

fn lease() -> SessionRealizationLease {
    SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "boot".into(),
        epoch: 1,
        // Deliberately unrelated to the Worker wall clock. The source returns
        // these original coordinates, not a locally rewritten expiration.
        expires_at_unix_ms: 60_000,
    }
}

fn install(host: &SharedHost, lease: SessionRealizationLease) {
    let proof = SessionRealizationLeaseReceipt::issued_at(lease, 0)
        .unwrap()
        .observed(awaken_authority_clock::authority_proof_started().unwrap());
    host.run_assembly
        .installed_sessions
        .install_realization("session", |r| {
            r.execution_mut().authority = SessionRealizationAuthority::Live(proof);
        });
}

#[tokio::test]
async fn provider_fence_source_decision_table_preserves_exact_live_authority() {
    // Receiver consolidation: every row now calls the original
    // ExecutionAuthority directly. Exact/stale Worker, Session, operation and
    // expiry coordinates still decide capture and GO; removing the Holding
    // forwarding method cannot add a grant or change these original effects.

    // Causes: C1 registered/direct topology; C2 Worker live/zero/closed;
    // C3 Session live/expired/closed/uninstalled/absent; C4 captured slot's two
    // allocation identities unchanged; C5 exact owner/incarnation/epoch and
    // operation; C6 expiry nondecreasing from initial through asserted/current.
    // Effect E1 = return the unchanged current Control fence iff every relevant
    // cause holds; E2 = typed rejection. No provider mutation is performed here.
    // P1/P2 exact/renewed (initial or intermediate assertion) -> E1.
    // Other named rows independently negate one
    // predicate -> E2 (MC/DC), with both topologies except Worker-only rows.
    // The ABA row restores byte-identical authority after removal, isolating
    // allocation identity from lease equality. Closed retains the exact bytes.
    // The same captured value also implements CurrentExecutionAuthority:
    // its callback must run once under live authority, never on denial. A bad
    // retry assertion cannot revoke the valid original operation's port.
    for registered in [false, true] {
        for rule in [
            "exact",
            "renewed",
            "renewed_assertion",
            "worker_closed",
            "worker_zero",
            "expired",
            "closed",
            "uninstalled",
            "removed",
            "slot_aba",
            "realization",
            "lifecycle",
            "owner",
            "incarnation",
            "epoch",
            "current_regressed",
            "asserted_future",
            "asserted_regressed",
            "asserted_operation",
            "asserted_owner",
            "asserted_incarnation",
            "asserted_epoch",
        ] {
            if !registered && rule.starts_with("worker_") {
                continue;
            }
            let worker = Arc::new(Worker(Mutex::new(Some(Duration::from_secs(30)))));
            let mut host = dependencies().await;
            if registered {
                host = host.with_execution_authority(worker.clone());
            }
            let host = (host).build().expect("validated Host dependencies");
            let initial = lease().sandbox_effect_fence("create").unwrap();
            install(&host, lease());
            let source = host
                .realization_executor
                .authority()
                .provider_effect_authority("session", &initial)
                .unwrap();
            assert_eq!(source.current_effect_fence(&initial).unwrap(), initial);
            let mut asserted = initial.clone();
            let mut current = lease();
            match rule {
                "worker_closed" => *worker.0.lock().unwrap() = None,
                "worker_zero" => *worker.0.lock().unwrap() = Some(Duration::ZERO),
                "removed" => {
                    host.run_assembly.installed_sessions.remove("session");
                }
                "slot_aba" => {
                    let old = host
                        .run_assembly
                        .installed_sessions
                        .remove("session")
                        .unwrap();
                    host.run_assembly
                        .installed_sessions
                        .install_realization("session", |r| {
                            r.execution_mut().authority =
                                old.realization.execution().authority.clone();
                        });
                }
                "closed" => {
                    host.run_assembly
                        .installed_sessions
                        .close_realization("session", None)
                        .expect("installed Sessions");
                }
                "uninstalled" => host
                    .run_assembly
                    .installed_sessions
                    .realizations()
                    .update_realization("session", |r| {
                        r.execution_mut().authority = SessionRealizationAuthority::Uninstalled;
                    })
                    .expect("installed Sessions"),
                "expired" => host
                    .run_assembly
                    .installed_sessions
                    .realizations()
                    .update_realization("session", |r| {
                        r.execution_mut().authority = SessionRealizationAuthority::Live(
                            awaken_session_realization::test_support::test_expired_observed_lease(
                                current.clone(),
                            ),
                        );
                    })
                    .expect("installed Sessions"),
                "realization" | "lifecycle" => host
                    .run_assembly
                    .installed_sessions
                    .realizations()
                    .update_realization("session", |r| {
                        if rule == "realization" {
                            r.execution_mut().phase = Arc::new(tokio::sync::Mutex::new(()));
                        } else {
                            r.environment_mut().lifecycle = Arc::new(tokio::sync::Mutex::new(()));
                        }
                    })
                    .expect("installed Sessions"),
                "asserted_future" => asserted.renew(),
                "asserted_regressed" => asserted.tamper(|w| w.expires_at_unix_ms -= 1),
                "asserted_operation" => asserted.tamper(|w| w.operation_id.push_str("-other")),
                "asserted_owner" => asserted.tamper(|w| w.owner.push_str("-other")),
                "asserted_incarnation" => {
                    asserted.tamper(|w| w.runtime_incarnation.push_str("-other"))
                }
                "asserted_epoch" => asserted.reassign(),
                _ => {
                    match rule {
                        "exact" => {}
                        "renewed" | "renewed_assertion" => {
                            current.expires_at_unix_ms += 30_000;
                            if rule == "renewed_assertion" {
                                asserted.tamper(|w| w.expires_at_unix_ms += 15_000);
                            }
                        }
                        "current_regressed" => current.expires_at_unix_ms -= 1,
                        "owner" => current.owner.push_str("-other"),
                        "incarnation" => current.runtime_incarnation.push_str("-other"),
                        "epoch" => current.epoch += 1,
                        _ => unreachable!(),
                    }
                    install(&host, current.clone());
                }
            }
            let result = source.current_effect_fence(&asserted);
            assert_eq!(
                result.is_ok(),
                matches!(rule, "exact" | "renewed" | "renewed_assertion"),
                "{registered}/{rule}"
            );
            if let Ok(fence) = result {
                assert_eq!(fence, current.sandbox_effect_fence("create").unwrap());
            }
            let mut effects = 0;
            let result = source.with_current(&mut |remaining| {
                effects += 1;
                assert!(remaining > Duration::ZERO && remaining <= Duration::from_secs(60));
                if registered {
                    assert!(remaining <= Duration::from_secs(30));
                }
                Ok(())
            });
            let allowed = matches!(rule, "exact" | "renewed" | "renewed_assertion")
                || rule.starts_with("asserted_");
            assert_eq!(result.is_ok(), allowed, "{registered}/{rule}/effect");
            assert_eq!(
                effects,
                usize::from(allowed),
                "{registered}/{rule}/effect count"
            );

            // The SAME decision table now drives real acknowledgement code,
            // not only the fence projection adapter. C7 empty/new/replay: all
            // must reauthorize, and only a first nonempty ack may drain. The
            // 60s Control epoch is intentionally expired in Worker UTC; local
            // proof, not that unrelated wall clock, decides this boundary.
            let evidence = [
                awaken_provisioning_contract::MemoryMaterializationEvidence::new(
                    "store",
                    "/memory",
                    vec![],
                )
                .unwrap(),
            ];
            for materials in [&[][..], evidence.as_slice()] {
                let ack = awaken_provisioning_contract::MemoryReconciliationAck::default();
                let mut drained = 0;
                for _ in 0..2 {
                    let result = ack.acknowledge(&asserted, &source, materials, materials, |_| {
                        drained += 1;
                        if registered {
                            assert!(worker.0.try_lock().is_err());
                        }
                        Ok(())
                    });
                    assert_eq!(
                        result.is_ok(),
                        matches!(rule, "exact" | "renewed" | "renewed_assertion"),
                        "{registered}/{rule}/ack"
                    );
                }
                assert_eq!(
                    drained,
                    usize::from(
                        !materials.is_empty()
                            && matches!(rule, "exact" | "renewed" | "renewed_assertion")
                    ),
                    "{registered}/{rule}/drain"
                );
            }
        }
    }
}

#[tokio::test]
async fn provider_fence_source_capture_rejects_absent_slot_and_invalid_identity() {
    // P3 missing slot or invalid operation -> capture error, with no slot
    // creation. Empty identity is independent of existence: test both states.
    let host = host().await;
    let valid = lease().sandbox_effect_fence("create").unwrap();
    assert!(
        host.realization_executor
            .authority()
            .provider_effect_authority("session", &valid)
            .is_err()
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read("session", |_| ())
            .expect("installed Sessions")
            .is_none()
    );
    // An effect fence with an incomplete identity never reaches capture: the
    // one admission refuses it before a value exists, installed slot or not.
    assert!(
        valid.with_persisted(|w| w.operation_id.clear()).is_err(),
        "blank operation identity is refused at construction"
    );
    install(&host, lease());

    // C1 slot exists; C2 the capture has an installed live-kind lease;
    // C3 initial generation matches; C4 initial expiry does not exceed it.
    // Each false row rejects at capture, before any operation can use a port.
    // This is distinct from the late replacement table above.
    for rule in [
        "uninstalled",
        "closed",
        "owner",
        "incarnation",
        "epoch",
        "expiry",
    ] {
        let host = self::host().await;
        install(&host, lease());
        let mut initial = lease().sandbox_effect_fence("create").unwrap();
        match rule {
            "uninstalled" => host
                .run_assembly
                .installed_sessions
                .realizations()
                .update_realization("session", |r| {
                    r.execution_mut().authority = SessionRealizationAuthority::Uninstalled;
                })
                .expect("installed Sessions"),
            "closed" => {
                host.run_assembly
                    .installed_sessions
                    .close_realization("session", None)
                    .expect("installed Sessions");
            }
            "owner" => initial.tamper(|w| w.owner.push_str("-other")),
            "incarnation" => initial.tamper(|w| w.runtime_incarnation.push_str("-other")),
            "epoch" => initial.reassign(),
            "expiry" => initial.renew(),
            _ => unreachable!(),
        }
        assert!(
            host.realization_executor
                .authority()
                .provider_effect_authority("session", &initial)
                .is_err(),
            "{rule}"
        );
    }
}

#[tokio::test]
async fn provider_fence_source_cannot_retain_the_host_or_authorize_after_its_drop() {
    // W1 capture a source while the exact Session exists; W2 drop its Host.
    // Both the fence query and synchronous effect must deny, with zero effects.
    // Owned opaque return + Weak slot reference makes this a real drop check,
    // not a source that merely borrows a Host kept alive by the test itself.
    let host = host().await;
    install(&host, lease());
    let initial = lease().sandbox_effect_fence("create").unwrap();
    let source = host
        .realization_executor
        .authority()
        .provider_effect_authority("session", &initial)
        .unwrap();
    drop(host);
    assert!(source.current_effect_fence(&initial).is_err());
    assert!(
        source
            .with_current(&mut |_| panic!("dropped Host"))
            .is_err()
    );
}

#[tokio::test]
async fn provider_fence_source_waiting_for_worker_rechecks_slot_after_revocation() {
    // Temporal P4: request captured -> blocked acquiring Worker guard -> Session
    // closes/recreates -> Worker guard released -> callback must reject. P5 the
    // identical ordering with a same-generation renewal returns its new fence.
    // A channel establishes that the callback started; no scheduler sleep or
    // cached pre-wait Session decision can make a stale response acceptable.
    for rule in ["closed", "slot_aba", "renewed"] {
        struct GatedWorker {
            gate: Mutex<()>,
            entered: std::sync::mpsc::Sender<()>,
        }
        impl CurrentExecutionAuthority for GatedWorker {
            fn with_current(
                &self,
                effect: &mut dyn FnMut(Duration) -> io::Result<()>,
            ) -> io::Result<()> {
                self.entered.send(()).unwrap();
                let _guard = self.gate.lock().unwrap();
                effect(Duration::from_secs(30))
            }
        }
        let (entered, receiver) = std::sync::mpsc::channel();
        let worker = Arc::new(GatedWorker {
            gate: Mutex::new(()),
            entered,
        });
        let host = (dependencies()
            .await
            .with_execution_authority(worker.clone()))
        .build()
        .expect("validated Host dependencies");
        install(&host, lease());
        let initial = lease().sandbox_effect_fence("create").unwrap();
        let source = host
            .realization_executor
            .authority()
            .provider_effect_authority("session", &initial)
            .unwrap();
        std::thread::scope(|scope| {
            let guard = worker.gate.lock().unwrap();
            let pending = scope.spawn(|| source.current_effect_fence(&initial));
            receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            match rule {
                "closed" => {
                    host.run_assembly
                        .installed_sessions
                        .close_realization("session", None)
                        .expect("installed Sessions");
                }
                "slot_aba" => {
                    let old = host
                        .run_assembly
                        .installed_sessions
                        .remove("session")
                        .unwrap();
                    host.run_assembly
                        .installed_sessions
                        .install_realization("session", |r| {
                            r.execution_mut().authority =
                                old.realization.execution().authority.clone();
                        });
                }
                "renewed" => {
                    let mut current = lease();
                    current.expires_at_unix_ms += 30_000;
                    install(&host, current);
                }
                _ => unreachable!(),
            }
            drop(guard);
            let result = pending.join().unwrap();
            assert_eq!(result.is_ok(), rule == "renewed", "{rule}");
            if let Ok(fence) = result {
                assert_eq!(fence.expires_at_unix_ms(), 90_000);
            }
        });
    }
}

#[derive(Default)]
struct Boundary {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    calls: std::sync::atomic::AtomicUsize,
}

impl Boundary {
    async fn pause(&self) {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
    }
}

#[async_trait::async_trait]
impl crate::cache_volume::CacheVolumeInitializer for Boundary {
    async fn initialize(
        &self,
        _: &crate::cache_volume::CacheVolumeWarmup,
    ) -> Result<(), crate::cache_volume::CacheVolumeError> {
        self.pause().await;
        Ok(())
    }
}

#[derive(Default)]
struct ProjectionProbe {
    boundary: Boundary,
    observation_boundary: Option<Arc<Boundary>>,
    replies: Mutex<Vec<SandboxEffectFence>>,
}

#[async_trait::async_trait]
impl awaken_container_contract::ContainerEnvironmentProvider for ProjectionProbe {
    fn adoption_layout(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        awaken_container_contract::ContainerAdoptionLayout,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    fn physical_incarnation(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<String, awaken_provisioning_contract::SandboxError> {
        awaken_sandbox_container::container_physical_incarnation(handle)
    }

    fn sandbox_capabilities(&self) -> awaken_provisioning_contract::SandboxCapabilities {
        awaken_provisioning_contract::SandboxCapabilities {
            isolation: awaken_provisioning_contract::IsolationClass::Container,
            tool_transparent: true,
            path_fidelity: true,
            ..Default::default()
        }
    }

    async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }

    async fn create_unattached_environment(
        &self,
        _: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<
        Arc<dyn awaken_container_contract::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        // An unattached creation has no authority source to forward: the
        // provider port cannot even be handed one.
        self.boundary.pause().await;
        Err(awaken_provisioning_contract::SandboxError::backend(
            "create_unattached_environment",
            "projection probe finished",
        ))
    }

    async fn adopt_unattached_environment(
        &self,
        _: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
    ) -> Result<
        Arc<dyn awaken_container_contract::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        panic!("creation must not adopt")
    }

    async fn observe_environment(
        &self,
        _: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
        basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
    ) -> Result<
        awaken_provisioning_contract::SandboxObservation,
        awaken_provisioning_contract::SandboxError,
    > {
        assert!(
            matches!(
                basis,
                awaken_provisioning_contract::SandboxObservationBasis::Effect { .. }
            ),
            "fenced adoption observes under its effect fence"
        );
        if let Some(boundary) = &self.observation_boundary {
            boundary.pause().await;
        }
        Ok(awaken_provisioning_contract::SandboxObservation::Ready)
    }

    async fn adopt_environment_for_effect(
        &self,
        adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
        fence: &SandboxEffectFence,
        source: &dyn CurrentExecutionAuthority,
    ) -> Result<
        Arc<dyn awaken_container_contract::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        // Same projection recorder for both provider entrances, no mock lease.
        self.create_environment_for_effect(
            adoption.spec,
            fence,
            source,
            awaken_container_contract::EnvironmentRealizationIntent::Create,
        )
        .await
    }

    async fn create_environment_for_effect(
        &self,
        _: &awaken_provisioning_contract::SandboxSpec,
        fence: &SandboxEffectFence,
        source: &dyn CurrentExecutionAuthority,
        _: awaken_container_contract::EnvironmentRealizationIntent,
    ) -> Result<
        Arc<dyn awaken_container_contract::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        self.boundary.pause().await;
        let current = source.current_effect_fence(fence)?;
        self.replies.lock().unwrap().push(current);
        Err(awaken_provisioning_contract::SandboxError::backend(
            "create_environment_for_effect",
            "projection probe finished",
        ))
    }

    async fn prepare_terminal_environment_for_effect(
        &self,
        _: &awaken_provisioning_contract::SandboxSpec,
        _: Option<&awaken_provisioning_contract::SandboxHandle>,
        _: Option<&SandboxEffectFence>,
        _: &SandboxEffectFence,
        _: &dyn CurrentExecutionAuthority,
    ) -> Result<
        Option<Arc<dyn awaken_container_contract::ContainerEnvironment>>,
        awaken_provisioning_contract::SandboxError,
    > {
        panic!("creation must not prepare terminal cleanup")
    }
}

#[tokio::test]
async fn adoption_forwards_original_authority_across_observation_and_provider_waits() {
    // H1-H14: direct/registered x observation/adoption Pending x Session close,
    // same-byte slot ABA, renewal, or registered Worker exhaustion. C1 authority
    // must be captured BEFORE observation. E1 renewal alone reaches one current
    // projection with original Control coordinates; E2 loss/ABA reaches zero.
    // The probe deliberately returns an error after recording, so no physical
    // Container or publication is fabricated. Provider tests own real resource
    // retention/retry; this table owns Host forwarding and original-slot capture.
    for registered in [false, true] {
        for wait in ["observation", "provider"] {
            for change in ["closed", "aba", "renewed", "worker_zero"] {
                if !registered && change == "worker_zero" {
                    continue;
                }
                let worker = Arc::new(Worker(Mutex::new(Some(Duration::from_secs(30)))));
                let mut host = dependencies().await;
                if registered {
                    host = host.with_execution_authority(worker.clone());
                }
                let host = (host).build().expect("validated Host dependencies");
                install(&host, lease());
                let observation = Arc::new(Boundary::default());
                let probe = Arc::new(ProjectionProbe {
                    observation_boundary: Some(observation.clone()),
                    ..Default::default()
                });
                let provider = awaken_session_environment::SandboxEnvironmentProvider::container(
                    probe.clone(),
                    vec![],
                    Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
                    "unused",
                )
                .into_session_provider();
                let spec = host
                    .realization_executor
                    .staging()
                    .sandbox_spec_for_provider("session", &provider)
                    .expect("installed Sessions");
                let handle =
                    crate::host::tests::terminal_fixture_container_handle("session", &spec);
                let binding = serde_json::to_string(&handle).unwrap();
                let intent = awaken_session_state_contract::SessionEnvironmentEffectIntent::new(
                    "session",
                    awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt,
                    Some(lease()),
                )
                .from_binding(&binding);
                let initial = lease().sandbox_effect_fence(intent.effect_id()).unwrap();
                let effect = AuthorizedSessionEnvironmentEffect {
                    provider_authority: Some(Arc::new(host.realization_executor.authority().provider_effect_authority("session", &initial).unwrap())),
                    provider_fence: Some(initial),
                    authorization:
                        awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized { intent: Box::new(intent) },
                };
                let lifecycle = host
                    .realization_executor
                    .sessions()
                    .lock_lifecycle("session")
                    .await
                    .unwrap();
                let adopt = host
                    .realization_executor
                    .environments()
                    .adopt_bound_session_environment_under_lifecycle(
                        lifecycle.held(),
                        host.realization_executor.peers(),
                        "session",
                        &binding,
                        &provider,
                        None,
                        SessionEnvironmentUnavailablePolicy::Reject,
                        Some(&effect),
                    );
                let transition = async {
                    observation.entered.notified().await;
                    if wait == "provider" {
                        observation.release.notify_one();
                        probe.boundary.entered.notified().await;
                    }
                    match change {
                        "closed" => {
                            host.run_assembly
                                .installed_sessions
                                .close_realization("session", None)
                                .expect("installed Sessions");
                        }
                        "aba" => {
                            let old = host
                                .run_assembly
                                .installed_sessions
                                .remove("session")
                                .unwrap();
                            host.run_assembly.installed_sessions.install_realization(
                                "session",
                                |r| {
                                    r.execution_mut().authority =
                                        old.realization.execution().authority.clone();
                                },
                            );
                        }
                        "renewed" => {
                            let mut renewed = lease();
                            renewed.expires_at_unix_ms += 30_000;
                            install(&host, renewed);
                        }
                        "worker_zero" => *worker.0.lock().unwrap() = Some(Duration::ZERO),
                        _ => unreachable!(),
                    }
                    observation.release.notify_one();
                    probe.boundary.release.notify_one();
                };
                let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(adopt, transition)
                })
                .await
                .unwrap();
                assert!(result.is_err(), "probe never publishes");
                let replies = probe.replies.lock().unwrap();
                assert_eq!(
                    replies.len(),
                    usize::from(change == "renewed"),
                    "{registered}/{wait}/{change}"
                );
                if let Some(fence) = replies.first() {
                    assert_eq!(fence.expires_at_unix_ms(), 90_000);
                }
            }
        }
    }
}

#[tokio::test]
async fn provider_fence_source_is_wired_across_preparation_and_provider_awaits() {
    // Cause/effect sequence table for the REAL Host -> Session provider seam:
    // W1 closed initially -> no cache/provider call.
    // W2/W3 close/same-byte ABA during cache await -> cache completes, provider
    // never starts. W4/W5 same transitions during provider await -> forwarded
    // source rejects, no returned fence. W6 same-generation renewal during that
    // await -> exactly one unchanged current fence reaches the provider.
    // W7 unfenced legacy create -> no invented source or Session authority.
    // Barriers force each ordering. Probe intentionally creates no physical
    // environment: lower provider replay/cleanup tests own those effects; this
    // must not be presented as real Container or cross-clock I/O qualification.
    for rule in [
        "initial_closed",
        "warmup_closed",
        "warmup_aba",
        "provider_closed",
        "provider_aba",
        "provider_renewed",
        "provider_unfenced",
    ] {
        let warmup = Arc::new(Boundary::default());
        let host = (dependencies()
            .await
            .with_cache_volume_initializer(warmup.clone()))
        .build()
        .expect("validated Host dependencies");
        install(&host, lease());
        if rule == "provider_unfenced" {
            host.run_assembly.installed_sessions.remove("session");
        }
        let probe = Arc::new(ProjectionProbe::default());
        let provider = awaken_session_environment::SandboxEnvironmentProvider::container(
            probe.clone(),
            vec![],
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            "unused",
        )
        .into_session_provider();
        let mut spec = awaken_session_environment::test_support::spec();
        spec.scope = "session".into();
        let directory = tempfile::tempdir().unwrap();
        spec.mounts
            .push(awaken_provisioning_contract::MountRequirement {
                mount_id: "cache".into(),
                source: awaken_provisioning_contract::MountSource::CacheVolume {
                    location: awaken_provisioning_contract::CacheVolumeLocation::HostPath {
                        path: directory.path().to_string_lossy().into_owned(),
                    },
                    key: "fixture".into(),
                },
                mount_path: "/cache".into(),
                access: awaken_provisioning_contract::MountAccess::ReadWrite,
                lifetime: awaken_provisioning_contract::MountLifetime::Durable,
                required: true,
            });
        let initial = lease().sandbox_effect_fence("create").unwrap();
        let authority = (rule != "provider_unfenced").then(|| {
            host.realization_executor
                .authority()
                .provider_effect_authority("session", &initial)
                .unwrap()
        });
        if rule == "initial_closed" {
            host.run_assembly
                .installed_sessions
                .close_realization("session", None)
                .expect("installed Sessions");
        }
        let create = awaken_session_realization::test_support::create_session_environment(
            &host.realization_executor,
            &provider,
            &spec,
            (rule != "provider_unfenced").then_some(&initial),
            authority
                .as_ref()
                .map(|source| source as &dyn CurrentExecutionAuthority),
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        );
        let transition = async {
            if rule == "initial_closed" {
                return;
            }
            warmup.entered.notified().await;
            if rule.starts_with("provider_") {
                warmup.release.notify_one();
                probe.boundary.entered.notified().await;
            }
            if rule.ends_with("_closed") {
                host.run_assembly
                    .installed_sessions
                    .close_realization("session", None)
                    .expect("installed Sessions");
            } else if rule.ends_with("_aba") {
                let old = host
                    .run_assembly
                    .installed_sessions
                    .remove("session")
                    .unwrap();
                host.run_assembly
                    .installed_sessions
                    .install_realization("session", |r| {
                        r.execution_mut().authority = old.realization.execution().authority.clone();
                    });
            } else if rule != "provider_unfenced" {
                let mut current = lease();
                current.expires_at_unix_ms += 30_000;
                install(&host, current);
            }
            if rule.starts_with("provider_") {
                probe.boundary.release.notify_one();
            } else {
                warmup.release.notify_one();
            }
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(create, transition)
        })
        .await
        .expect(rule);
        let error = match result {
            Ok(_) => panic!("{rule}: probe never creates physical state"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(
                if matches!(rule, "provider_renewed" | "provider_unfenced") {
                    "projection probe finished"
                } else if rule.ends_with("_aba") {
                    "installed Session was replaced"
                } else {
                    "proof is no longer live"
                }
            ),
            "{rule}: {error}"
        );
        use std::sync::atomic::Ordering::SeqCst;
        assert_eq!(
            warmup.calls.load(SeqCst),
            usize::from(rule != "initial_closed"),
            "{rule}"
        );
        assert_eq!(
            probe.boundary.calls.load(SeqCst),
            usize::from(rule.starts_with("provider_")),
            "{rule}"
        );
        let replies = probe.replies.lock().unwrap();
        assert_eq!(
            replies.len(),
            usize::from(rule == "provider_renewed"),
            "{rule}"
        );
        if let Some(reply) = replies.first() {
            let mut current = lease();
            current.expires_at_unix_ms += 30_000;
            assert_eq!(*reply, current.sandbox_effect_fence("create").unwrap());
        }
    }
}

#[tokio::test]
async fn environment_receipt_refreshes_only_the_captured_live_generation() {
    // Cause/effect graph: C1 direct/registered Worker; C2 exact/renewed lease;
    // C3 owner/incarnation/epoch/expiry changed; C4 closed/removed/ABA slot;
    // C5 Worker closed/zero. E1 exact or monotonic renewal => original receipt,
    // identical effect/binding/context; E2 all loss/reassignment rows => reject.
    // R1 exact, R2 renewed (old Control expiry can already be stale), R3 each
    // independent negated authority predicate; no sleeps or wall-clock guesses.
    for registered in [false, true] {
        for rule in [
            "exact",
            "renewed",
            "owner",
            "incarnation",
            "epoch",
            "regressed",
            "closed",
            "removed",
            "aba",
            "worker_zero",
        ] {
            if !registered && rule == "worker_zero" {
                continue;
            }
            let worker = Arc::new(Worker(Mutex::new(Some(Duration::from_secs(30)))));
            let mut host = dependencies().await;
            if registered {
                host = host.with_execution_authority(worker.clone());
            }
            let host = (host).build().expect("validated Host dependencies");
            install(&host, lease());
            let intent = awaken_session_state_contract::SessionEnvironmentEffectIntent::new(
                "session",
                awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
                Some(lease()),
            )
            .for_environment("immutable-config");
            let initial = lease().sandbox_effect_fence(intent.effect_id()).unwrap();
            let authority = host
                .realization_executor
                .authority()
                .provider_effect_authority("session", &initial)
                .unwrap();
            let effect = AuthorizedSessionEnvironmentEffect {
                authorization:
                    awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized { intent: Box::new(intent) },
                provider_fence: Some(initial),
                provider_authority: Some(Arc::new(authority)),
            };
            let mut current = lease();
            match rule {
                "renewed" => {
                    current.expires_at_unix_ms = 120_000;
                    install(&host, current.clone());
                }
                "owner" => {
                    current.owner = "other".into();
                    install(&host, current);
                }
                "incarnation" => {
                    current.runtime_incarnation = "other".into();
                    install(&host, current);
                }
                "epoch" => {
                    current.epoch += 1;
                    install(&host, current);
                }
                "regressed" => {
                    current.expires_at_unix_ms -= 1;
                    install(&host, current);
                }
                "closed" => {
                    host.run_assembly
                        .installed_sessions
                        .close_realization("session", None)
                        .expect("installed Sessions");
                }
                "removed" => {
                    host.run_assembly.installed_sessions.remove("session");
                }
                "aba" => {
                    let old = host
                        .run_assembly
                        .installed_sessions
                        .remove("session")
                        .unwrap();
                    host.run_assembly
                        .installed_sessions
                        .install_realization("session", |r| {
                            r.execution_mut().authority =
                                old.realization.execution().authority.clone();
                        });
                }
                "worker_zero" => {
                    *worker.0.lock().unwrap() = Some(Duration::ZERO);
                }
                _ => {}
            }
            let receipt = host
                .realization_executor
                .authority()
                .current_environment_receipt("binding".into(), &effect);
            assert_eq!(
                receipt.is_ok(),
                rule == "exact" || rule == "renewed",
                "{registered}/{rule}"
            );
            if let Ok(receipt) = receipt {
                receipt.verify().unwrap();
                assert_eq!(receipt.effect_id, effect.authorization.intent().effect_id());
                assert_eq!(receipt.binding, "binding");
                assert_eq!(
                    receipt.environment_fingerprint.as_deref(),
                    Some("immutable-config")
                );
                assert_eq!(
                    receipt.realization.as_ref().unwrap().expires_at_unix_ms,
                    60_000,
                    "R1/R2 original expiry is immutable even under renewed authority"
                );
                if rule == "renewed" {
                    assert!(!session_resource::realization_lease_is_live_at(
                        60_000, 90_000
                    ));
                    assert!(session_resource::realization_lease_authorizes(
                        &session_resource::SessionRealizationLease {
                            expires_at_unix_ms: 120_000,
                            ..lease()
                        },
                        receipt.realization.as_ref().unwrap(),
                        90_000
                    ));
                }
            }
        }
    }
}
