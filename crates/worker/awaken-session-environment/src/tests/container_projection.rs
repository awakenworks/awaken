//! Container rebuild refusal, managed file-tree reprojection, and observation fences.

use super::*;
use awaken_sandbox_container::ContainerHandleCodec as _;
use awaken_session_environment_contract::{EnvironmentAdoption as _, EnvironmentRealizer as _};

#[tokio::test]
async fn readonly_container_observation_preserves_exact_results_and_effect_basis() {
    // RO1 graph/table: C1 source_prepared false/true; C2 unattached/effect
    // basis; C3 each physical/retention result or transport/timeout/backend/
    // authority/unsupported/conflict error. Every C1*C2*C3 row delegates once,
    // preserves the exact typed result and fence, and performs no write.
    // RO2 Recovery/TerminalCleanup are outside this read-only role and never
    // reach the backend. Metadata uses the same projection as execution;
    // upstream retirement tests, rather than this adapter, decide permission.
    let fake = Arc::new(FakeContainerProvider::default());
    let observer = ContainerEnvironmentObservation::new(fake.clone(), Vec::new());
    let full = SandboxEnvironmentProvider::container(
        fake.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/unused",
    )
    .into_session_provider();
    let effective = observer.effective_spec(&spec()).unwrap();
    assert_eq!(effective, full.realizer().effective_spec(&spec()).unwrap());
    assert_eq!(observer.capabilities(), full.realizer().capabilities());
    let handle = current_fake_container_handle(&effective);
    let fence = pc::SandboxEffectFence::new("retention", "owner", "boot", 3, u64::MAX).unwrap();
    let results = [
        Ok(pc::SandboxObservation::Provisioning { deadline: None }),
        Ok(pc::SandboxObservation::RetainedOriginal {
            physical_incarnation: "original".into(),
        }),
        Ok(pc::SandboxObservation::Ready),
        Ok(pc::SandboxObservation::Terminal {
            physical_incarnation: "original".into(),
        }),
        Ok(pc::SandboxObservation::Disposing {
            physical_incarnation: "original".into(),
        }),
        Ok(pc::SandboxObservation::DefinitivelyUnavailable {
            physical_incarnation: None,
        }),
        Ok(pc::SandboxObservation::Incompatible {
            reason: "different original".into(),
        }),
        Err(pc::SandboxError::transport("response lost")),
        Err(pc::SandboxError::not_found("original missing")),
        Err(pc::SandboxError::gone("original closed")),
        Err(pc::SandboxError::not_ready("source converging")),
        Err(pc::SandboxError::timeout("read expired")),
        Err(pc::SandboxError::backend("read", "API unavailable")),
        Err(pc::SandboxError::unauthorized("read forbidden")),
        Err(pc::SandboxError::capacity_unavailable("read capacity")),
        Err(pc::SandboxError::fence_rejected("different authority")),
        Err(pc::SandboxError::invalid("malformed evidence")),
        Err(pc::SandboxError::unsupported("unsupported backend")),
        Err(pc::SandboxError::conflict("different source")),
    ];
    let mut reads = 0;
    for prepared in [false, true] {
        for fenced in [false, true] {
            for result in &results {
                *fake.observation.lock().unwrap() = Some(result.clone());
                let basis = if fenced {
                    pc::SandboxObservationBasis::Effect {
                        spec: &effective,
                        effect_fence: &fence,
                    }
                } else {
                    pc::SandboxObservationBasis::Unattached
                };
                let purpose = pc::SandboxObservationPurpose::WorkerRetirement {
                    source_prepared: prepared,
                };
                assert_eq!(
                    observer
                        .observe_effective(&effective, &handle, None, basis, purpose,)
                        .await,
                    *result
                );
                reads += 1;
                assert_eq!(fake.observation_calls.load(Ordering::SeqCst), reads);
                if fenced {
                    assert_eq!(
                        fake.observations.lock().unwrap().last(),
                        Some(&(purpose, fence.clone()))
                    );
                }
            }
        }
    }
    for purpose in [
        pc::SandboxObservationPurpose::Recovery,
        pc::SandboxObservationPurpose::TerminalCleanup,
    ] {
        assert!(matches!(
            observer
                .observe_effective(
                    &effective,
                    &handle,
                    None,
                    pc::SandboxObservationBasis::Unattached,
                    purpose,
                )
                .await,
            Err(pc::SandboxError::Unsupported(_))
        ));
    }
    assert_eq!(fake.observation_calls.load(Ordering::SeqCst), reads, "RO2");
    assert_eq!(fake.creates.load(Ordering::SeqCst), 0);
    assert_eq!(fake.adopts.load(Ordering::SeqCst), 0);
    assert_eq!(fake.disposals.load(Ordering::SeqCst), 0);
    assert_eq!(fake.hand_spawns.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn readonly_container_roles_refuse_all_effects_before_backend_access() {
    // RO3 decision table: C1 unattached/fenced effect; C2 Create/Rebuild,
    // adopt or retained rebind. Every C1*C2 row returns Unsupported before
    // backend access, independently of handle contents or live authority.
    // RO4 layout and closed-evidence validation also cannot become admission
    // or cleanup permission. One execution fixture supplies a retained object;
    // its counters form the unchanged baseline, never retirement evidence.
    let fake = Arc::new(FakeContainerProvider::default());
    let full = SandboxEnvironmentProvider::container(
        fake.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/unused",
    )
    .into_session_provider();
    let effective = full.realizer().effective_spec(&spec()).unwrap();
    let retained = full
        .realizer()
        .create_effective_for_effect(
            &effective,
            pc::SandboxEffect::Unattached,
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .unwrap();
    let handle = retained.lifecycle().handle();
    let observer = ContainerEnvironmentObservation::new(fake.clone(), Vec::new());
    let fence = pc::SandboxEffectFence::new("retention", "owner", "boot", 3, u64::MAX).unwrap();
    let authority = pc::test_support::EffectAuthority::new(&fence);
    for fenced in [false, true] {
        let effect = || {
            if fenced {
                pc::SandboxEffect::Fenced {
                    fence: &fence,
                    authority: &authority,
                }
            } else {
                pc::SandboxEffect::Unattached
            }
        };
        for kind in [
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
            awaken_session_environment_contract::EnvironmentRealizationKind::Rebuild,
        ] {
            assert!(matches!(
                observer
                    .create_effective_for_effect(&effective, effect(), Some(&handle), kind,)
                    .await,
                Err(pc::SandboxError::Unsupported(_))
            ));
        }
        assert!(matches!(
            observer
                .adopt_effective_for_effect(&effective, &handle, effect(),)
                .await,
            Err(pc::SandboxError::Unsupported(_))
        ));
        assert!(matches!(
            observer
                .rebind_retained(&effective, &handle, &retained, effect(),)
                .await,
            Err(pc::SandboxError::Unsupported(_))
        ));
    }
    assert!(
        full.adoption()
            .effective_adoption_layout(&effective, &handle)
            .is_ok()
    );
    let closed = pc::SandboxObservation::DefinitivelyUnavailable {
        physical_incarnation: Some(handle.container_physical_incarnation().unwrap()),
    };
    assert!(
        full.adoption()
            .validate_closed_observation(&handle, &closed)
            .is_ok()
    );
    assert!(matches!(
        observer.effective_adoption_layout(&effective, &handle),
        Err(pc::SandboxError::Unsupported(_))
    ));
    assert!(matches!(
        observer.validate_closed_observation(&handle, &closed),
        Err(pc::SandboxError::Unsupported(_))
    ));
    assert_eq!(fake.creates.load(Ordering::SeqCst), 1, "RO3 baseline only");
    assert_eq!(fake.adopts.load(Ordering::SeqCst), 0);
    assert_eq!(fake.observation_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fake.disposals.load(Ordering::SeqCst), 0);
    assert_eq!(fake.hand_spawns.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unattached_container_rebuild_is_refused_before_any_provider_call() {
    // U1 an unattached (unowned) Session asks for a container Rebuild: the Host
    // refuses with FenceRejected and the provider sees zero creations, rather
    // than silently degrading the rebuild into an ordinary unfenced create.
    // U2 the same unattached Create reaches the unattached provider entrance.
    let fake = Arc::new(FakeContainerProvider::default());
    let provider = SandboxEnvironmentProvider::container(
        fake.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider();
    let spec = provider.realizer().effective_spec(&spec()).unwrap();
    let source = pc::SandboxHandle::new("container", "source");
    let rebuilt = provider
        .realizer()
        .create_effective_for_effect(
            &spec,
            awaken_provisioning_contract::SandboxEffect::Unattached,
            Some(&source),
            awaken_session_environment_contract::EnvironmentRealizationKind::Rebuild,
        )
        .await;
    assert!(
        matches!(rebuilt, Err(pc::SandboxError::FenceRejected(_))),
        "U1"
    );
    assert_eq!(fake.creates.load(Ordering::SeqCst), 0, "U1 zero creations");
    provider
        .realizer()
        .create_effective_for_effect(
            &spec,
            awaken_provisioning_contract::SandboxEffect::Unattached,
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .expect("U2");
    assert_eq!(fake.creates.load(Ordering::SeqCst), 1, "U2");
}

#[tokio::test]
async fn adopted_container_reprojects_the_current_managed_file_tree() {
    // Causes: A1 an adopted Container retains a valid physical Sandbox but its
    // live-input tree is empty after a Worker handoff; A2 the frozen Resource
    // manifest names a read-only Managed File; A3 another mount is outside the
    // Managed Files root. Effects: A1+A2 restores the exact file bytes through
    // the existing Sandbox attach port; A3 is not reattached as a File. Rules
    // R1=A1+A2 -> restored, R2=A1+A3 -> untouched. This catches the former
    // Container no-op that let an active manifest coexist with an empty Pod.
    let fake = Arc::new(FakeContainerProvider::default());
    let provider = SandboxEnvironmentProvider::container(
        fake.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider();
    let spec = provider.realizer().effective_spec(&spec()).unwrap();
    let created = provider
        .realizer()
        .create_effective_for_effect(
            &spec,
            awaken_provisioning_contract::SandboxEffect::Unattached,
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .unwrap();
    let mounted = pc::MountRequirement {
        mount_id: "managed-file".into(),
        source: pc::MountSource::InlineBytes {
            contents: b"customer evidence".to_vec(),
            content_hash: None,
        },
        mount_path: "/mnt/session/uploads/customer/evidence.txt".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::Session,
        required: true,
    };
    let outside = pc::MountRequirement {
        mount_id: "other-mount".into(),
        mount_path: "/workspace/other.txt".into(),
        ..mounted.clone()
    };
    fake.shared
        .lock()
        .unwrap()
        .insert(mounted.mount_path.clone(), b"customer evidence".to_vec());
    let handle = created.lifecycle().handle();
    drop(created);
    fake.shared.lock().unwrap().remove(&mounted.mount_path);
    let adopted = provider
        .adoption()
        .adopt_effective_for_effect(
            &spec,
            &handle,
            awaken_provisioning_contract::SandboxEffect::Unattached,
        )
        .await
        .expect("adopt the same container after the input tree was lost");
    adopted
        .projection()
        .reconcile_adopted_mounts(&[mounted.clone(), outside])
        .await
        .expect("R1/R2 adopted Managed Files are reprojected");
    let files = fake.shared.lock().unwrap();
    assert_eq!(
        files.get(&mounted.mount_path).map(Vec::as_slice),
        Some(b"customer evidence".as_slice()),
        "R1"
    );
    assert!(!files.contains_key("/workspace/other.txt"), "R2");
}

#[tokio::test]
async fn host_observation_preserves_purpose_and_exact_fence() {
    // H1 ordinary recovery and H2 terminal cleanup pass the canonical Sandbox
    // purpose plus the unchanged fence through the same adapter. The producer
    // and expected provider value use the sole enum; hard-coding Recovery must
    // still fail H2. H3 provider failure remains an error, never a stopped
    // receipt. No creation/adoption/disposal. Type-path acceptance and rejection
    // are paired compile cases on the owning EnvironmentAdoption role.
    let fake = Arc::new(FakeContainerProvider::default());
    *fake.observation.lock().unwrap() =
        Some(Ok(awaken_provisioning_contract::SandboxObservation::Ready));
    let provider = SandboxEnvironmentProvider::container(
        fake.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider();
    let spec = provider.realizer().effective_spec(&spec()).unwrap();
    let handle = current_fake_container_handle(&spec);
    let fence = pc::SandboxEffectFence::new("observe", "owner", "boot", 1, u64::MAX).unwrap();
    for (purpose, container_purpose) in [
        (
            awaken_provisioning_contract::SandboxObservationPurpose::Recovery,
            awaken_provisioning_contract::SandboxObservationPurpose::Recovery,
        ),
        (
            awaken_provisioning_contract::SandboxObservationPurpose::TerminalCleanup,
            awaken_provisioning_contract::SandboxObservationPurpose::TerminalCleanup,
        ),
    ] {
        assert_eq!(
            provider
                .adoption()
                .observe_effective(
                    &spec,
                    &handle,
                    None,
                    pc::SandboxObservationBasis::Effect {
                        spec: &spec,
                        effect_fence: &fence
                    },
                    purpose,
                )
                .await
                .unwrap(),
            pc::SandboxObservation::Ready
        );
        assert_eq!(
            fake.observations.lock().unwrap().last(),
            Some(&(container_purpose, fence.clone()))
        );
    }
    *fake.observation.lock().unwrap() = None;
    assert!(
        provider
            .adoption()
            .observe_effective(
                &spec,
                &handle,
                None,
                pc::SandboxObservationBasis::Effect {
                    spec: &spec,
                    effect_fence: &fence,
                },
                awaken_provisioning_contract::SandboxObservationPurpose::TerminalCleanup
            )
            .await
            .is_err()
    );
    assert_eq!(fake.creates.load(Ordering::SeqCst), 0);
    assert_eq!(fake.adopts.load(Ordering::SeqCst), 0);
    assert_eq!(fake.disposals.load(Ordering::SeqCst), 0);
}

#[test]
fn terminal_data_readability_follows_the_physical_tier() {
    // Decision table: T1 Workdir and T2 Namespace keep direct filesystem
    // access after their marker is terminal => readable; T3 a Container's
    // file reads need a running process => not readable.
    use awaken_session_environment_contract::EnvironmentTerminalRealization as _;
    let base = tempfile::tempdir().unwrap();
    assert!(
        SandboxEnvironmentProvider::workdir(base.path()).terminal_data_is_readable(),
        "T1"
    );
    assert!(
        SandboxEnvironmentProvider::namespace_with_agent_stderr(
            base.path(),
            false,
            Arc::new(UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        )
        .terminal_data_is_readable(),
        "T2"
    );
    assert!(
        !SandboxEnvironmentProvider::container(
            Arc::new(FakeContainerProvider::default()),
            Vec::new(),
            Arc::new(UnusedHandExecutorFactory),
            "/bin/sh",
        )
        .terminal_data_is_readable(),
        "T3"
    );
}
