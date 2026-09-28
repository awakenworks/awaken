//! Container rebuild refusal, managed file-tree reprojection, and observation fences.

use super::*;

#[tokio::test]
async fn unattached_container_rebuild_is_refused_before_any_provider_call() {
    // U1 an unattached (unowned) Session asks for a container Rebuild: the Host
    // refuses with FenceRejected and the provider sees zero creations, rather
    // than silently degrading the rebuild into an ordinary unfenced create.
    // U2 the same unattached Create reaches the unattached provider entrance.
    let fake = Arc::new(FakeContainerProvider::default());
    let provider = SessionEnvironmentProvider::container(
        fake.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    );
    let spec = provider.effective_spec(&spec()).unwrap();
    let rebuilt = provider
        .create_effective_for_effect(
            &spec,
            crate::session_environment::ProviderEffect::Unattached,
            None,
            awaken_container_contract::EnvironmentRealizationIntent::Rebuild {
                source: Box::new(pc::SandboxHandle::new("container", "source")),
            },
        )
        .await;
    assert!(
        matches!(rebuilt, Err(pc::SandboxError::FenceRejected(_))),
        "U1"
    );
    assert_eq!(fake.creates.load(Ordering::SeqCst), 0, "U1 zero creations");
    provider
        .create_effective_for_effect(
            &spec,
            crate::session_environment::ProviderEffect::Unattached,
            None,
            awaken_container_contract::EnvironmentRealizationIntent::Create,
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
    let provider = SessionEnvironmentProvider::container(
        fake.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    );
    let spec = provider.effective_spec(&spec()).unwrap();
    let created = provider
        .create_effective_for_effect(
            &spec,
            crate::session_environment::ProviderEffect::Unattached,
            None,
            awaken_container_contract::EnvironmentRealizationIntent::Create,
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
    let handle = created.handle();
    drop(created);
    fake.shared.lock().unwrap().remove(&mounted.mount_path);
    let adopted = provider
        .adopt_effective(&spec, &handle)
        .await
        .expect("adopt the same container after the input tree was lost");
    adopted
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
    // H1 ordinary recovery and H2 terminal cleanup pass their distinct purpose
    // plus the unchanged fence through the same adapter. H3 provider failure
    // remains an error, never a stopped receipt. No creation/adoption/disposal.
    let fake = Arc::new(FakeContainerProvider::default());
    fake.shared
        .lock()
        .unwrap()
        .insert("__ready_observation".into(), Vec::new());
    let provider = SessionEnvironmentProvider::container(
        fake.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    );
    let spec = provider.effective_spec(&spec()).unwrap();
    let handle = current_fake_container_handle(&spec);
    let fence = pc::SandboxEffectFence::new("observe", "owner", "boot", 1, u64::MAX).unwrap();
    for purpose in [
        awaken_container_contract::ContainerObservationPurpose::Recovery,
        awaken_container_contract::ContainerObservationPurpose::TerminalCleanup,
    ] {
        assert_eq!(
            provider
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
            Some(&(purpose, fence.clone()))
        );
    }
    fake.shared.lock().unwrap().remove("__ready_observation");
    assert!(
        provider
            .observe_effective(
                &spec,
                &handle,
                None,
                pc::SandboxObservationBasis::Effect {
                    spec: &spec,
                    effect_fence: &fence,
                },
                awaken_container_contract::ContainerObservationPurpose::TerminalCleanup
            )
            .await
            .is_err()
    );
    assert_eq!(fake.creates.load(Ordering::SeqCst), 0);
    assert_eq!(fake.adopts.load(Ordering::SeqCst), 0);
    assert_eq!(fake.disposals.load(Ordering::SeqCst), 0);
}
