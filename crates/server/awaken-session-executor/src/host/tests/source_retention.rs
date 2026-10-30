//! Worker source-retention observations and exact-generation background activity.

use super::terminal_memory::terminal_memory_projection;
use super::*;
use awaken_session_realization::environment_owner::{
    BoundSessionEnvironment, BoundSessionEnvironmentIdentity, LegacyDirectEnvironmentProvenance,
    LegacyEnvironmentEffect, RetiringEnvironmentOwner, RetiringSessionEnvironment,
    SessionEnvironmentOwner, SessionEnvironmentRetirementCause,
};
use awaken_session_run_delivery::{BackgroundRuns, BackgroundWorkClass};

#[tokio::test]
async fn worker_retirement_consumes_only_explicit_retention_and_preserves_provider_failures() {
    // SR1 cause/effect table through real RunAssembly -> EnvironmentHolding:
    // C1 provider observation; C2 root preparation already durable; C3 provider
    // failure class. Unprepared requires RetainedOriginal; physical Ready,
    // Terminal, Disposing or absence alone never proves independent retention.
    // Prepared keeps the existing replay for every compatible physical fact;
    // Incompatible always refuses. Errors never become permission: transport,
    // timeout/backend keep Unavailable; authority/unsupported keep classified
    // Internal. Every row preserves the projection and performs just one read.
    // These synthetic provider facts qualify the consumer, not real storage;
    // Kubernetes HTTP tests own the concrete identity/guard/PVC evidence.
    async fn read(
        outcome: Result<pc::SandboxObservation, pc::SandboxError>,
        prepared: bool,
    ) -> Result<bool, awaken_session_types::RunError> {
        let (host, events) = source_retention_host(outcome, None).await;
        let id = "source-retention-read";
        let handle =
            terminal_fixture_container_handle(id, &host.realization_executor.sandbox_spec(id));
        let mut projection = remote_terminal_cleanup_projection();
        projection.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: serde_json::to_string(&handle).unwrap(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        let before = serde_json::to_value(&projection).unwrap();
        let result = retention_roles(&host)
            .observe_source_retention(
                &awaken_session_types::SessionId::from(id),
                &projection,
                prepared,
            )
            .await;
        assert_eq!(serde_json::to_value(&projection).unwrap(), before);
        assert_eq!(*events.lock().unwrap(), ["observe-source-retention"]);
        result
    }
    for (observed, unprepared, prepared) in [
        (
            pc::SandboxObservation::RetainedOriginal {
                physical_incarnation: "pod-a".into(),
            },
            true,
            true,
        ),
        (pc::SandboxObservation::Ready, false, true),
        (
            pc::SandboxObservation::Terminal {
                physical_incarnation: "pod-a".into(),
            },
            false,
            true,
        ),
        (
            pc::SandboxObservation::Disposing {
                physical_incarnation: "pod-a".into(),
            },
            false,
            true,
        ),
        (
            pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: Some("pod-a".into()),
            },
            false,
            true,
        ),
        (
            pc::SandboxObservation::Incompatible {
                reason: "foreign".into(),
            },
            false,
            false,
        ),
    ] {
        for (is_prepared, expected) in [(false, unprepared), (true, prepared)] {
            assert_eq!(
                read(Ok(observed.clone()), is_prepared).await.unwrap(),
                expected,
                "SR1 {observed:?} prepared={is_prepared}"
            );
        }
    }
    use awaken_session_types::RunErrorKind::{Internal, Unavailable};
    for (failure, kind, code) in [
        (
            pc::SandboxError::transport("lost response"),
            Unavailable,
            "environment_unavailable",
        ),
        (
            pc::SandboxError::timeout("read deadline"),
            Unavailable,
            "environment_unavailable",
        ),
        (
            pc::SandboxError::backend("read", "offline"),
            Unavailable,
            "environment_unavailable",
        ),
        (
            pc::SandboxError::unauthorized("denied"),
            Internal,
            "environment_rejected",
        ),
        (
            pc::SandboxError::unsupported("provider"),
            Internal,
            "environment_rejected",
        ),
    ] {
        for prepared in [false, true] {
            let error = read(Err(failure.clone()), prepared).await.unwrap_err();
            assert_eq!((error.kind, error.code.as_str()), (kind, code));
        }
    }
}

const RETENTION_SESSION: &str = "source-retention-background";

// These reads execute no Run or Session application. Select the complete public
// Standalone topology once per host and retain/clone its original role lifetime;
// repeated observations must not reconfigure startup or bypass AlreadyBound.
fn retention_roles(host: &crate::ExecutorComponents) -> crate::WorkerRoles {
    crate::WorkerRoles::of(
        host,
        awaken_session_run_delivery::SessionApplications::Standalone,
    )
    .expect("retention fixture selects one complete non-executing topology")
}

struct SourceRetentionFixture {
    host: Arc<crate::ExecutorComponents>,
    roles: crate::WorkerRoles,
    projection: awaken_session_realization_contract::FrozenSessionProjection,
    owner: BoundSessionEnvironment,
    events: Arc<Mutex<Vec<&'static str>>>,
}

async fn source_retention_host(
    outcome: Result<pc::SandboxObservation, pc::SandboxError>,
    probe: Option<Arc<TerminalFenceRefreshProbe>>,
) -> (
    Arc<crate::ExecutorComponents>,
    Arc<Mutex<Vec<&'static str>>>,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        restored_disposals: Mutex::new(Vec::new()),
        events: events.clone(),
        scenario: TerminalRecoveryScenario::Observation(outcome),
        fence_refresh_probe: probe,
    });
    let host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_session_container_provider(
            provider,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
        )
        .build()
        .unwrap();
    (host, events)
}

async fn source_retention_fixture(
    outcome: Result<pc::SandboxObservation, pc::SandboxError>,
    probe: Option<Arc<TerminalFenceRefreshProbe>>,
    source_probe: Option<Arc<TerminalFenceRefreshProbe>>,
) -> SourceRetentionFixture {
    let (host, events) = source_retention_host(outcome, probe).await;
    // The original fenced-adoption fixture gives the source the exact Session
    // handle its canonical decoder requires. The observation provider remains
    // separate and read-only; source setup events never become read evidence.
    let source = awaken_session_environment::SandboxEnvironmentProvider::container(
        Arc::new(TerminalRecoveryProvider {
            restored_disposals: Mutex::new(Vec::new()),
            events: Arc::new(Mutex::new(Vec::new())),
            scenario: TerminalRecoveryScenario::LiveMemory,
            fence_refresh_probe: source_probe,
        }),
        Vec::new(),
        Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
        "unused-hand",
    )
    .into_session_provider();
    let spec = host.realization_executor.sandbox_spec(RETENTION_SESSION);
    let handle = terminal_fixture_container_handle(RETENTION_SESSION, &spec);
    assert_eq!(handle.sandbox_id, RETENTION_SESSION);
    let fence = pc::SandboxEffectFence::new(
        "retention-fixture-create",
        "retention-fixture",
        "retention-fixture:incarnation",
        1,
        u64::MAX,
    )
    .unwrap();
    let environment = Arc::new(
        source
            .adoption()
            .adopt_effective_for_effect(
                &spec,
                &handle,
                pc::SandboxEffect::Fenced {
                    fence: &fence,
                    authority: &pc::test_support::EffectAuthority::new(&fence),
                },
            )
            .await
            .unwrap(),
    );
    let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(RETENTION_SESSION),
        1,
        u64::MAX,
        "retention-environment",
        "retention-image",
    );
    assert_ne!(generation.id(), environment.lifecycle().handle().sandbox_id);
    let owner = BoundSessionEnvironment {
        identity: BoundSessionEnvironmentIdentity::Durable {
            effect_id: "retention-fixture-create".into(),
            generation: generation.clone(),
        },
        binding: binding.clone(),
        environment,
    };
    let mut projection = remote_terminal_cleanup_projection();
    projection.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding,
        effect_id: Some("retention-fixture-create".into()),
        generation: Some(generation),
        idle_since_unix_ms: None,
    };
    install_retention_owner(&host, Some(owner.clone()));
    let roles = retention_roles(&host);
    SourceRetentionFixture {
        host,
        roles,
        projection,
        owner,
        events,
    }
}

fn install_retention_owner(
    host: &crate::ExecutorComponents,
    owner: Option<BoundSessionEnvironment>,
) {
    host.run_assembly
        .installed_sessions
        .install_realization(RETENTION_SESSION, |realization| {
            realization.environment_mut().owner = owner.map_or(
                SessionEnvironmentOwner::Vacant,
                SessionEnvironmentOwner::Resident,
            );
        });
}

impl SourceRetentionFixture {
    fn background(&self) -> Arc<BackgroundRuns> {
        self.host.resources.memory.background()
    }

    fn shared_work(&self) -> BackgroundWorkClass {
        BackgroundWorkClass::SharedEnvironment {
            session_id: RETENTION_SESSION.into(),
            generation_id: self.owner.activity_generation_id(),
        }
    }

    async fn hold_work(&self, class: BackgroundWorkClass) -> Arc<tokio::sync::Notify> {
        let release = Arc::new(tokio::sync::Notify::new());
        let released = release.clone();
        self.background()
            .spawn(class, async move { released.notified().await })
            .await;
        release
    }

    async fn observe(&self, prepared: bool) -> Result<bool, awaken_session_types::RunError> {
        self.roles
            .observe_source_retention(
                &awaken_session_types::SessionId::from(RETENTION_SESSION),
                &self.projection,
                prepared,
            )
            .await
    }
}

fn retained_original() -> Result<pc::SandboxObservation, pc::SandboxError> {
    Ok(pc::SandboxObservation::RetainedOriginal {
        physical_incarnation: "retained-source".into(),
    })
}

#[tokio::test]
async fn worker_retirement_matches_only_the_original_projected_source_at_entry() {
    // SR6 cause/effect table reuses the Environment owner's projection lowering
    // and installation identity decision. Durable exact identity, or exact
    // binding+generation with another effect (the existing post-Adopt stutter),
    // admits the read without relabeling the owner. A different generation or
    // binding refuses before provider I/O. Legacy DurableBinding Known/Absent
    // remains exact; Direct(receipt) is aggregate-Unowned and cannot become a
    // DurableBinding from matching bytes alone. Suspending compares its original
    // source, never its suspend operation. A generation missing its effect is
    // an original lowering error, not retention permission. No row changes the
    // projection, owner, Arc, handle, lease or physical execution authority.
    for case in [
        "exact",
        "effect-stutter",
        "generation",
        "binding",
        "missing-effect",
        "legacy-known",
        "legacy-absent",
        "legacy-effect",
        "legacy-direct",
        "suspending",
        "suspending-stutter",
        "suspending-legacy",
        "suspending-binding",
        "suspending-generation",
    ] {
        use awaken_session_state_contract::SessionEnvironmentState as State;
        let mut fixture = source_retention_fixture(retained_original(), None, None).await;
        if case.starts_with("suspending") {
            let lease = crate::session_resource::SessionRealizationLease {
                owner: "retention-fixture".into(),
                runtime_incarnation: "retention-fixture:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: u64::MAX,
            };
            let (state, _, _) = disposing_terminal_environment(
                RETENTION_SESSION,
                &lease,
                &fixture.owner.environment.lifecycle().handle(),
                "retention-checkpoint",
            );
            let (_, effect, _, generation) = state.suspend_source().unwrap();
            fixture.owner.identity = BoundSessionEnvironmentIdentity::Durable {
                effect_id: effect.into(),
                generation: generation.clone(),
            };
            fixture.projection.environment = state;
        }
        let accepted = match case {
            "exact" | "suspending" => true,
            "effect-stutter" => {
                let State::Resident { effect_id, .. } = &mut fixture.projection.environment else {
                    unreachable!()
                };
                *effect_id = Some("post-adopt-receipt".into());
                true
            }
            "generation" => {
                let State::Resident { generation, .. } = &mut fixture.projection.environment else {
                    unreachable!()
                };
                *generation = Some(awaken_session_state_contract::SandboxGeneration::new(
                    &awaken_session_types::SessionId::from(RETENTION_SESSION),
                    2,
                    u64::MAX,
                    "retention-environment",
                    "retention-image",
                ));
                false
            }
            "binding" => {
                let State::Resident { binding, .. } = &mut fixture.projection.environment else {
                    unreachable!()
                };
                binding.push(' ');
                false
            }
            "missing-effect" => {
                let State::Resident { effect_id, .. } = &mut fixture.projection.environment else {
                    unreachable!()
                };
                *effect_id = None;
                false
            }
            "legacy-known" | "legacy-absent" | "legacy-effect" | "legacy-direct" => {
                let State::Resident {
                    effect_id,
                    generation,
                    ..
                } = &mut fixture.projection.environment
                else {
                    unreachable!()
                };
                *generation = None;
                *effect_id = (case == "legacy-known" || case == "legacy-effect")
                    .then(|| "legacy-source".into());
                let provenance = if case == "legacy-direct" {
                    LegacyDirectEnvironmentProvenance::Direct(
                        awaken_session_state_contract::SessionEnvironmentReceipt::new(
                            RETENTION_SESSION,
                            awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
                            fixture.owner.binding.clone(),
                            None,
                        ),
                    )
                } else {
                    LegacyDirectEnvironmentProvenance::DurableBinding {
                        binding: fixture.owner.binding.clone(),
                        effect: if case == "legacy-absent" {
                            LegacyEnvironmentEffect::Absent
                        } else {
                            LegacyEnvironmentEffect::Known("legacy-source".into())
                        },
                    }
                };
                fixture.owner.identity = BoundSessionEnvironmentIdentity::LegacyDirect(provenance);
                if case == "legacy-effect" {
                    *effect_id = Some("another-source".into());
                }
                case == "legacy-known" || case == "legacy-absent"
            }
            "suspending-stutter" => {
                let State::Suspending {
                    source_effect_id, ..
                } = &mut fixture.projection.environment
                else {
                    unreachable!()
                };
                **source_effect_id = "post-adopt-source".into();
                true
            }
            "suspending-legacy" => {
                let State::Suspending {
                    source_effect_id, ..
                } = &mut fixture.projection.environment
                else {
                    unreachable!()
                };
                source_effect_id.clear();
                fixture.owner.identity = BoundSessionEnvironmentIdentity::LegacyDirect(
                    LegacyDirectEnvironmentProvenance::DurableBinding {
                        binding: fixture.owner.binding.clone(),
                        effect: LegacyEnvironmentEffect::Absent,
                    },
                );
                true
            }
            "suspending-binding" => {
                let State::Suspending { source_binding, .. } = &mut fixture.projection.environment
                else {
                    unreachable!()
                };
                source_binding.push(' ');
                false
            }
            "suspending-generation" => {
                let State::Suspending { generation, .. } = &mut fixture.projection.environment
                else {
                    unreachable!()
                };
                *generation = awaken_session_state_contract::SandboxGeneration::new(
                    &awaken_session_types::SessionId::from(RETENTION_SESSION),
                    2,
                    u64::MAX,
                    "environment-disposing",
                    "image-disposing",
                );
                false
            }
            _ => unreachable!(),
        };
        install_retention_owner(&fixture.host, Some(fixture.owner.clone()));
        let before = serde_json::to_value(&fixture.projection).unwrap();
        let result = fixture.observe(false).await;
        if case == "missing-effect" {
            assert!(result.is_err(), "SR6 {case}");
        } else {
            assert_eq!(result.unwrap(), accepted, "SR6 {case}");
        }
        assert_eq!(
            fixture.events.lock().unwrap().len(),
            usize::from(accepted),
            "SR6 provider {case}"
        );
        assert_eq!(serde_json::to_value(&fixture.projection).unwrap(), before);
        let current = fixture
            .host
            .realization_executor
            .sessions()
            .read_realization(RETENTION_SESSION, |r| {
                r.environment().owner.terminal_bound_environment()
            })
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            current.exact_matches(&fixture.owner),
            "SR6 no relabel {case}"
        );
    }
}

#[tokio::test]
async fn worker_retirement_keeps_the_exact_shared_generation_until_background_releases() {
    // SR2/SR3 cause/effect table: C1 original owner is Resident/Bound-Retiring;
    // C2 source_prepared=false/true; C3 its exact durable activity generation
    // is busy/released. R1=C3 busy => false before provider I/O for all C1/C2;
    // R2=C3 released => the original provider proof decides, without changing
    // owner, projection, lease or compute-capacity counts. The durable key is
    // deliberately different from the physical handle. BackgroundRuns' own
    // a_poisoned_activity_map_keeps_every_generation_busy test owns B3; this
    // integration consumes that same busy result, never defaults poison idle.
    for retiring in [false, true] {
        for prepared in [false, true] {
            let fixture = source_retention_fixture(retained_original(), None, None).await;
            if retiring {
                fixture
                    .host
                    .run_assembly
                    .installed_sessions
                    .install_realization(RETENTION_SESSION, |realization| {
                        realization.environment_mut().owner =
                            SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                                cause: SessionEnvironmentRetirementCause::RealizationRevocation,
                                owned: RetiringEnvironmentOwner::Bound(fixture.owner.clone()),
                            });
                    });
            }
            let before = serde_json::to_value(&fixture.projection).unwrap();
            let release = fixture.hold_work(fixture.shared_work()).await;
            let background = fixture.background();
            assert!(
                !fixture.observe(prepared).await.unwrap(),
                "SR2 retiring={retiring}, prepared={prepared}",
            );
            assert!(
                fixture.events.lock().unwrap().is_empty(),
                "SR2 before provider"
            );
            release.notify_one();
            assert!(
                background
                    .quiesce_shared_environment(
                        RETENTION_SESSION,
                        &fixture.owner.activity_generation_id(),
                        std::time::Duration::from_secs(2),
                    )
                    .await,
                "SR3 exact release"
            );
            assert!(
                fixture.observe(prepared).await.unwrap(),
                "SR3 original proof"
            );
            assert_eq!(
                *fixture.events.lock().unwrap(),
                ["observe-source-retention"]
            );
            assert_eq!(serde_json::to_value(&fixture.projection).unwrap(), before);
        }
    }
}

#[tokio::test]
async fn worker_retirement_ignores_background_work_outside_its_exact_generation() {
    // SR4 partitions C1=another Session, another generation (including the
    // physical sandbox id), ExternalDurable, EphemeralCache or unattached
    // Environment. Each leaves the exact bound generation idle, so one original
    // provider observation still decides retention while unrelated work lives.
    for class in 0..6 {
        let fixture = source_retention_fixture(retained_original(), None, None).await;
        let work = match class {
            0 => BackgroundWorkClass::SharedEnvironment {
                session_id: "another-session".into(),
                generation_id: fixture.owner.activity_generation_id(),
            },
            1 => BackgroundWorkClass::SharedEnvironment {
                session_id: RETENTION_SESSION.into(),
                generation_id: "another-generation".into(),
            },
            2 => BackgroundWorkClass::SharedEnvironment {
                session_id: RETENTION_SESSION.into(),
                generation_id: fixture.owner.environment.lifecycle().handle().sandbox_id,
            },
            3 => BackgroundWorkClass::ExternalDurable {
                durable_intent_id: "external".into(),
            },
            4 => BackgroundWorkClass::EphemeralCache,
            5 => BackgroundWorkClass::UnattachedEnvironment,
            _ => unreachable!(),
        };
        let release = fixture.hold_work(work).await;
        assert!(fixture.observe(false).await.unwrap(), "SR4 class={class}");
        assert_eq!(
            *fixture.events.lock().unwrap(),
            ["observe-source-retention"]
        );
        release.notify_one();
        assert!(
            fixture
                .background()
                .drain(std::time::Duration::from_secs(2))
                .await
        );
    }
}

#[tokio::test]
async fn worker_retirement_rechecks_original_owner_and_activity_after_provider_read() {
    // SR5 async decision table: provider blocks, then C1 exact work starts;
    // C2 byte-equal owner/handle gets a different Arc; C3 owner disappears;
    // C4 a cold owner appears; C5 bound generation changes; C6 owner stays exact.
    // R1 C1-C5 => false after one original read; R2 C6 => true. R3 an original
    // provider error keeps its typed classification even when C1 occurs. The
    // gates prove the observation window; timeouts only bound broken fixtures.
    for change in 0..7 {
        let probe = Arc::new(TerminalFenceRefreshProbe::default());
        let outcome = if change == 6 {
            Err(pc::SandboxError::transport("retention read lost"))
        } else {
            retained_original()
        };
        let fixture = source_retention_fixture(outcome, Some(probe.clone()), None).await;
        if change == 3 {
            install_retention_owner(&fixture.host, None);
        }
        let roles = fixture.roles.clone();
        let projection = fixture.projection.clone();
        let mut observation = tokio::spawn(async move {
            roles
                .observe_source_retention(
                    &awaken_session_types::SessionId::from(RETENTION_SESSION),
                    &projection,
                    false,
                )
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async {
                tokio::select! {
                    () = probe.retention_entered.notified() => {}
                    result = &mut observation => {
                        panic!("SR5 change={change}: observation ended before provider entry: {result:?}");
                    }
                }
            },
        )
        .await
        .unwrap();
        let release = match change {
            0 | 6 => Some(fixture.hold_work(fixture.shared_work()).await),
            1 => {
                let mut replacement = fixture.owner.clone();
                replacement.environment = Arc::new((*replacement.environment).clone());
                install_retention_owner(&fixture.host, Some(replacement));
                None
            }
            2 | 3 => {
                install_retention_owner(
                    &fixture.host,
                    (change == 3).then(|| fixture.owner.clone()),
                );
                None
            }
            4 => {
                let mut replacement = fixture.owner.clone();
                replacement.identity = BoundSessionEnvironmentIdentity::Durable {
                    effect_id: "replacement-effect".into(),
                    generation: awaken_session_state_contract::SandboxGeneration::new(
                        &awaken_session_types::SessionId::from(RETENTION_SESSION),
                        2,
                        u64::MAX,
                        "retention-environment",
                        "retention-image",
                    ),
                };
                install_retention_owner(&fixture.host, Some(replacement));
                None
            }
            5 => None,
            _ => unreachable!(),
        };
        probe.retention_release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), observation)
            .await
            .unwrap()
            .unwrap();
        if change == 6 {
            let error = result.unwrap_err();
            assert_eq!(
                error.kind,
                awaken_session_types::RunErrorKind::Unavailable,
                "SR5/R3"
            );
            assert_eq!(error.code, "environment_unavailable", "SR5/R3");
        } else {
            assert_eq!(result.unwrap(), change == 5, "SR5 change={change}");
        }
        assert_eq!(
            *fixture.events.lock().unwrap(),
            ["observe-source-retention"]
        );
        if let Some(release) = release {
            release.notify_one();
        }
        assert!(
            fixture
                .background()
                .drain(std::time::Duration::from_secs(2))
                .await
        );
    }
}

#[tokio::test]
async fn worker_retirement_rechecks_the_same_arcs_entry_handle_after_provider_read() {
    // SR7 causes: one exact Bound identity, binding and Arc remain installed;
    // a positive original provider read blocks; C1 the same live Environment
    // reserves one owned path, or C2 its handle stays unchanged. R1 C1 => false;
    // R2 C2 => true. The original exact_matches still reports true in R1 because
    // both clones read the same live handle. Removing only the separate entry
    // SandboxHandle comparison therefore changes R1, without changing any other
    // owner/busy check. Typed V2 equality proves the private probe changes only
    // owned_paths and retains every fingerprint and physical-incarnation field.
    for mutate in [false, true] {
        let observation_probe = Arc::new(TerminalFenceRefreshProbe::default());
        let source_probe = Arc::new(TerminalFenceRefreshProbe::default());
        let fixture = source_retention_fixture(
            retained_original(),
            Some(observation_probe.clone()),
            Some(source_probe.clone()),
        )
        .await;
        let environment = fixture.owner.environment.clone();
        let handle = environment.lifecycle().handle();
        // Default None preserves the older fixture's Unsupported result, even
        // when another (unselected) fence-refresh probe is already present.
        assert!(
            environment
                .projection()
                .reserve_owned_path("/unselected")
                .is_err()
        );
        *source_probe.retention_source_handle.lock().unwrap() = Some(handle.clone());
        let original = fixture.owner.clone();
        let roles = fixture.roles.clone();
        let projection = fixture.projection.clone();
        let observation = tokio::spawn(async move {
            roles
                .observe_source_retention(
                    &awaken_session_types::SessionId::from(RETENTION_SESSION),
                    &projection,
                    false,
                )
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            observation_probe.retention_entered.notified(),
        )
        .await
        .unwrap();
        let mut expected = handle
            .decode_provider_payload::<awaken_sandbox_container::ContainerSandboxHandleV2>()
            .unwrap();
        if mutate {
            environment
                .projection()
                .reserve_owned_path("/retention-during-read")
                .unwrap();
            expected.owned_paths.push("/retention-during-read".into());
            expected.owned_paths.sort();
            expected.owned_paths.dedup();
        }
        let current = fixture
            .host
            .realization_executor
            .sessions()
            .read_realization(RETENTION_SESSION, |r| {
                r.environment().owner.terminal_bound_environment()
            })
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&original.environment, &current.environment));
        assert_eq!(original.identity, current.identity);
        assert_eq!(original.binding, current.binding);
        assert!(
            original.exact_matches(&current),
            "SR7 same live handle on both clones"
        );
        let emitted = current.environment.lifecycle().handle();
        assert_eq!(emitted != handle, mutate, "SR7 independent entry value");
        assert_eq!(emitted.sandbox_id, handle.sandbox_id);
        assert_eq!(
            emitted
                .decode_provider_payload::<awaken_sandbox_container::ContainerSandboxHandleV2>()
                .unwrap(),
            expected
        );
        assert_eq!(emitted.restoration(), handle.restoration());
        assert_eq!(
            emitted.memory_materializations().unwrap(),
            handle.memory_materializations().unwrap()
        );
        observation_probe.retention_release.notify_one();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), observation,)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            !mutate,
            "SR7 mutate={mutate}"
        );
        assert_eq!(
            *fixture.events.lock().unwrap(),
            ["observe-source-retention"]
        );
    }
}

#[tokio::test]
async fn worker_retirement_refuses_unprepared_memory_in_current_or_retained_resources() {
    // LR4 decision table: current Memory only / retained previous Memory only,
    // with a Resident binding and source_prepared=false => refuse retirement
    // before provider access. Neither presence in just one projection nor Actor
    // drain proves its private buffers durable. This exercises the real
    // RunAssembly -> EnvironmentHolding read, using the existing projection
    // and Memory fixtures; no preparation or installed Session is needed.
    let host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
        .await
        .build()
        .expect("validated Host dependencies");
    let roles = retention_roles(&host);
    for current_memory in [false, true] {
        let mut projection = terminal_memory_projection();
        if current_memory {
            projection.previous_resource_manifest = None;
        } else {
            projection.resources = Default::default();
        }
        projection.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: serde_json::to_string(&pc::SandboxHandle::new(
                "retention-fixture",
                "live-memory",
            ))
            .unwrap(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        let before = serde_json::to_value(&projection).unwrap();
        assert!(
            !roles
                .observe_source_retention(
                    &awaken_session_types::SessionId::from("live-memory"),
                    &projection,
                    false,
                )
                .await
                .unwrap(),
            "unprepared Memory is not independent source retention: current={current_memory}",
        );
        assert_eq!(serde_json::to_value(&projection).unwrap(), before);
    }
}
