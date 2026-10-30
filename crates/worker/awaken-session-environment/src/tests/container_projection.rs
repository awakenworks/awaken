//! Container rebuild refusal, managed file-tree reprojection, and observation fences.

use super::*;

#[tokio::test]
async fn session_realizer_uses_the_selected_filesystem_providers_readiness_facts() {
    // Causes: C1 selected Workdir or Namespace adapter; C2 the existing
    // EnvironmentRealizer handle is consumed as canonical SandboxReadiness.
    // R1 Workdir -> exact Local capabilities and filesystem checkpoint format;
    // R2 Namespace -> exact Namespace capabilities and no checkpoint format.
    // R1 also probes the existing Workdir ancestor without creating a root.
    // Neither row creates a Hand or physical Sandbox. Namespace liveness still
    // belongs to its existing real-substrate test through the same adapter;
    // this table does not turn an unavailable isolator into a successful probe.
    let base = tempfile::tempdir().unwrap();
    let workdir_root = base.path().join("workdir-not-created");
    let namespace_root = base.path().join("namespace-not-created");
    for (row, adapter) in [
        ("R1", SandboxEnvironmentProvider::workdir(&workdir_root)),
        (
            "R2",
            SandboxEnvironmentProvider::namespace_with_agent_stderr(
                &namespace_root,
                false,
                Arc::new(UnusedHandExecutorFactory),
                "unused-readiness-hand",
                std::time::Duration::ZERO,
            ),
        ),
    ] {
        let (capabilities, formats) = match &adapter {
            SandboxEnvironmentProvider::Workdir { provider, .. } => (
                pc::SandboxReadiness::capabilities(provider),
                pc::SandboxReadiness::checkpoint_formats(provider),
            ),
            SandboxEnvironmentProvider::Namespace { provider, .. } => (
                pc::SandboxReadiness::capabilities(provider),
                pc::SandboxReadiness::checkpoint_formats(provider),
            ),
            SandboxEnvironmentProvider::Container { .. } => unreachable!(),
        };
        let selected = adapter.into_session_provider();
        let readiness: &dyn pc::SandboxReadiness = selected.realizer().as_ref();
        assert_eq!(readiness.capabilities(), capabilities, "{row}");
        assert_eq!(readiness.checkpoint_formats(), formats, "{row}");
        if row == "R1" {
            assert_eq!(readiness.probe_ready().await, Ok(()), "R1");
        }
        assert_eq!(
            readiness.checkpoint_formats(),
            if row == "R1" {
                vec!["awaken-fs-tar-v1".to_string()]
            } else {
                Vec::new()
            },
            "{row} physical checkpoint support"
        );
    }
    assert!(!workdir_root.exists(), "R1 no realization");
    assert!(!namespace_root.exists(), "R2 no realization");
}

#[tokio::test]
async fn session_realizer_probes_the_same_container_provider_without_caching_readiness() {
    // Cause/effect graph: C1 the selected physical provider changes its
    // readiness result and checkpoint formats after selection; C2 another
    // provider has different state. E1 each read observes only C1's current
    // facts; E2 errors keep their exact class and detail; E3 no creation,
    // adoption, disposal, renewal or Hand launch occurs. Decision table:
    // R3 Ready -> Ok; R4 NotReady -> exact error; R5 Transport -> exact error;
    // R6 Ready again -> Ok. Every row performs one fresh probe of the same
    // instance. Returning a cached success, constructing a replacement provider
    // or projecting an error as Ready fails this table.
    let physical = Arc::new(FakeContainerProvider::default());
    let foreign = Arc::new(FakeContainerProvider::default());
    let provider = SandboxEnvironmentProvider::container(
        physical.clone(),
        Vec::new(),
        Arc::new(UnusedHandExecutorFactory),
        "unused-readiness-hand",
    )
    .into_session_provider();
    let readiness: &dyn pc::SandboxReadiness = provider.realizer().as_ref();
    assert_eq!(readiness.capabilities(), physical.capabilities());
    for (index, expected) in [
        Ok(()),
        Err(pc::SandboxError::not_ready(
            "same provider is temporarily unavailable",
        )),
        Err(pc::SandboxError::transport(
            "same provider lost its channel",
        )),
        Ok(()),
    ]
    .into_iter()
    .enumerate()
    {
        *physical.readiness_error.lock().unwrap() = expected.clone().err();
        let formats = vec![format!("provider-checkpoint-{index}")];
        *physical.checkpoint_formats.lock().unwrap() = formats.clone();
        assert_eq!(readiness.probe_ready().await, expected, "R{}", index + 3);
        assert_eq!(readiness.checkpoint_formats(), formats, "R{}", index + 3);
        assert_eq!(
            physical.readiness_calls.load(Ordering::SeqCst),
            index + 1,
            "R{} one current observation",
            index + 3
        );
    }
    assert_eq!(foreign.readiness_calls.load(Ordering::SeqCst), 0, "C2");
    assert_eq!(physical.creates.load(Ordering::SeqCst), 0, "E3");
    assert_eq!(physical.adopts.load(Ordering::SeqCst), 0, "E3");
    assert_eq!(physical.disposals.load(Ordering::SeqCst), 0, "E3");
    assert_eq!(physical.renews.load(Ordering::SeqCst), 0, "E3");
    assert_eq!(physical.hand_spawns.load(Ordering::SeqCst), 0, "E3");
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
