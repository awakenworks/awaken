use super::*;
use crate::session_slot::SessionRealizationAuthority;
use awaken_provisioning_contract::{CurrentExecutionAuthority, SandboxEffectFence};
use awaken_sandbox_container::ContainerEffectFenceSource;
use awaken_session_contract::{SessionRealizationLease, SessionRealizationLeaseReceipt};
use std::{io, sync::Mutex, time::Duration};

struct Worker(Mutex<Option<Duration>>);

impl CurrentExecutionAuthority for Worker {
    fn with_current(&self, effect: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
        let remaining = self.0.lock().unwrap();
        effect(remaining.ok_or(io::ErrorKind::PermissionDenied)?)
    }
}

fn host() -> SharedHost {
    SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub")
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
        .observed(awaken_local_process::authority_proof_started().unwrap());
    host.session_slots.update("session", |slot| {
        slot.realization_authority = SessionRealizationAuthority::Live(proof);
    });
}

#[test]
fn provider_fence_source_decision_table_preserves_exact_live_authority() {
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
            let mut host = host();
            if registered {
                host = host.with_execution_authority(worker.clone());
            }
            let initial = lease().sandbox_effect_fence("create").unwrap();
            install(&host, lease());
            let source = host
                .session_environment_effect_fence_source("session", &initial)
                .unwrap();
            assert_eq!(source.current_effect_fence(&initial).unwrap(), initial);
            let mut asserted = initial.clone();
            let mut current = lease();
            match rule {
                "worker_closed" => *worker.0.lock().unwrap() = None,
                "worker_zero" => *worker.0.lock().unwrap() = Some(Duration::ZERO),
                "removed" => {
                    host.session_slots.remove("session");
                }
                "slot_aba" => {
                    let old = host.session_slots.remove("session").unwrap();
                    host.session_slots.update("session", |slot| {
                        slot.realization_authority = old.realization_authority;
                    });
                }
                "closed" => {
                    host.session_slots.close_realization("session", None);
                }
                "uninstalled" => host.session_slots.update("session", |slot| {
                    slot.realization_authority = SessionRealizationAuthority::Uninstalled;
                }),
                "expired" => host.session_slots.update("session", |slot| {
                    slot.realization_authority = SessionRealizationAuthority::Live(
                        crate::session_slot::test_expired_observed_lease(current.clone()),
                    );
                }),
                "realization" | "lifecycle" => host.session_slots.update("session", |slot| {
                    if rule == "realization" {
                        slot.realization = Arc::new(tokio::sync::Mutex::new(()));
                    } else {
                        slot.lifecycle = Arc::new(tokio::sync::Mutex::new(()));
                    }
                }),
                "asserted_future" => asserted.expires_at_unix_ms += 1,
                "asserted_regressed" => asserted.expires_at_unix_ms -= 1,
                "asserted_operation" => asserted.operation_id.push_str("-other"),
                "asserted_owner" => asserted.owner.push_str("-other"),
                "asserted_incarnation" => asserted.runtime_incarnation.push_str("-other"),
                "asserted_epoch" => asserted.epoch += 1,
                _ => {
                    match rule {
                        "exact" => {}
                        "renewed" | "renewed_assertion" => {
                            current.expires_at_unix_ms += 30_000;
                            if rule == "renewed_assertion" {
                                asserted.expires_at_unix_ms += 15_000;
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
        }
    }
}

#[test]
fn provider_fence_source_capture_rejects_absent_slot_and_invalid_identity() {
    // P3 missing slot or invalid operation -> capture error, with no slot
    // creation. Empty identity is independent of existence: test both states.
    let host = host();
    let valid = lease().sandbox_effect_fence("create").unwrap();
    assert!(
        host.session_environment_effect_fence_source("session", &valid)
            .is_err()
    );
    assert!(host.session_slots.read("session", |_| ()).is_none());
    let mut invalid = valid;
    invalid.operation_id.clear();
    assert!(
        host.session_environment_effect_fence_source("session", &invalid)
            .is_err()
    );
    install(&host, lease());
    assert!(
        host.session_environment_effect_fence_source("session", &invalid)
            .is_err()
    );

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
        let host = self::host();
        install(&host, lease());
        let mut initial = lease().sandbox_effect_fence("create").unwrap();
        match rule {
            "uninstalled" => host.session_slots.update("session", |slot| {
                slot.realization_authority = SessionRealizationAuthority::Uninstalled;
            }),
            "closed" => {
                host.session_slots.close_realization("session", None);
            }
            "owner" => initial.owner.push_str("-other"),
            "incarnation" => initial.runtime_incarnation.push_str("-other"),
            "epoch" => initial.epoch += 1,
            "expiry" => initial.expires_at_unix_ms += 1,
            _ => unreachable!(),
        }
        assert!(
            host.session_environment_effect_fence_source("session", &initial)
                .is_err(),
            "{rule}"
        );
    }
}

#[test]
fn provider_fence_source_cannot_retain_the_host_or_authorize_after_its_drop() {
    // W1 capture a source while the exact Session exists; W2 drop its Host.
    // Both the fence query and synchronous effect must deny, with zero effects.
    // Owned opaque return + Weak slot reference makes this a real drop check,
    // not a source that merely borrows a Host kept alive by the test itself.
    let host = host();
    install(&host, lease());
    let initial = lease().sandbox_effect_fence("create").unwrap();
    let source = host
        .session_environment_effect_fence_source("session", &initial)
        .unwrap();
    drop(host);
    assert!(source.current_effect_fence(&initial).is_err());
    assert!(
        source
            .with_current(&mut |_| panic!("dropped Host"))
            .is_err()
    );
}

#[test]
fn provider_fence_source_waiting_for_worker_rechecks_slot_after_revocation() {
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
        let host = host().with_execution_authority(worker.clone());
        install(&host, lease());
        let initial = lease().sandbox_effect_fence("create").unwrap();
        let source = host
            .session_environment_effect_fence_source("session", &initial)
            .unwrap();
        std::thread::scope(|scope| {
            let guard = worker.gate.lock().unwrap();
            let pending = scope.spawn(|| source.current_effect_fence(&initial));
            receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            match rule {
                "closed" => {
                    host.session_slots.close_realization("session", None);
                }
                "slot_aba" => {
                    let old = host.session_slots.remove("session").unwrap();
                    host.session_slots.update("session", |slot| {
                        slot.realization_authority = old.realization_authority;
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
                assert_eq!(fence.expires_at_unix_ms, 90_000);
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
    async fn initialize(&self, _: &crate::cache_volume::CacheVolumeWarmup) -> Result<(), String> {
        self.pause().await;
        Ok(())
    }
}

#[derive(Default)]
struct ProjectionProbe {
    boundary: Boundary,
    replies: Mutex<Vec<SandboxEffectFence>>,
}

#[async_trait::async_trait]
impl awaken_sandbox_container::ContainerEnvironmentProvider for ProjectionProbe {
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

    async fn create_environment(
        &self,
        _: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        panic!("fenced creation must not fall back to legacy create")
    }

    async fn adopt_environment(
        &self,
        _: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
    ) -> Result<
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        panic!("creation must not adopt")
    }

    async fn create_environment_for_effect(
        &self,
        _: &awaken_provisioning_contract::SandboxSpec,
        fence: Option<&SandboxEffectFence>,
        source: Option<&dyn ContainerEffectFenceSource>,
        _: awaken_sandbox_container::ContainerRealizationIntent,
    ) -> Result<
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        self.boundary.pause().await;
        if let Some(fence) = fence {
            let current = source
                .expect("Host must forward the captured source")
                .current_effect_fence(fence)?;
            self.replies.lock().unwrap().push(current);
        } else {
            assert!(
                source.is_none(),
                "unfenced creation invents no authority source"
            );
        }
        Err(awaken_provisioning_contract::SandboxError::new(
            "projection probe finished",
        ))
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
        let mut host = host();
        let warmup = Arc::new(Boundary::default());
        host.cache_volume_prewarmer =
            crate::cache_volume::CacheVolumePrewarmer::new(warmup.clone());
        install(&host, lease());
        if rule == "provider_unfenced" {
            host.session_slots.remove("session");
        }
        let probe = Arc::new(ProjectionProbe::default());
        let provider = crate::session_environment::SessionEnvironmentProvider::container(
            probe.clone(),
            vec![],
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "unused",
        );
        let mut spec = crate::session_environment::environment_test_spec();
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
        if rule == "initial_closed" {
            host.session_slots.close_realization("session", None);
        }
        let create = host.create_session_environment_with_provider_effect(
            "session",
            &provider,
            &spec,
            (rule != "provider_unfenced").then_some(&initial),
            None,
            awaken_sandbox_container::ContainerRealizationIntent::Create,
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
                host.session_slots.close_realization("session", None);
            } else if rule.ends_with("_aba") {
                let old = host.session_slots.remove("session").unwrap();
                host.session_slots.update("session", |slot| {
                    slot.realization_authority = old.realization_authority;
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
                    "slot was replaced"
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
