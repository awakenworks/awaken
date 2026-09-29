//! Terminal preparation reconstructs only exact physical cleanup: no live control
//! binding, no ordinary execution, and no rebinding of a closed owner.
use super::*;

#[tokio::test]
async fn terminal_control_reconstruction_does_not_require_a_live_binding() {
    // Control reconstruction table TC1: exact frozen spec/handle/fence plus a
    // Terminal or Disposing observation -> preserve the recorded control
    // incarnation for exact physical cleanup, with zero live binding calls.
    // TC2 Ready still needs a real binding; a missing binding rejects. TC3
    // ordinary adoption of Terminal remains refused. This provider fixture
    // asserts routing/identity/absence of live control, not a real Pod receipt.
    for disposing in [false, true] {
        let runtime = Arc::new(
            FakeRuntime::default()
                .with_sandbox_control()
                .with_live_input_projection()
                .with_native_memory_archive(Vec::new()),
        );
        let provider = provider(runtime.clone());
        let mut desired = spec(if disposing {
            "closed-control-disposing"
        } else {
            "closed-control-terminal"
        });
        desired.mounts.clear();
        desired
            .control_services
            .insert(SandboxControlServiceKind::RepositoryGitCredential);
        let original = provider.create_container(&desired).await.unwrap();
        let handle = pc::Sandbox::handle(&original);
        let original_control = handle
            .container_payload()
            .unwrap()
            .sandbox_control_incarnation
            .clone();
        let incarnation = handle.container_physical_incarnation().unwrap().to_owned();
        drop(original);
        let fence = pc::SandboxEffectFence::new(
            "terminal-control-cleanup",
            "owner",
            "runtime",
            2,
            u64::MAX,
        )
        .unwrap();
        let authority = pc::test_support::EffectAuthority::new(&fence);
        {
            let mut state = runtime.st.lock().unwrap();
            state.control_binding_missing = true;
            state.control_binding_calls.clear();
        }
        runtime.set_observation(if disposing {
            pc::SandboxObservation::Disposing {
                physical_incarnation: incarnation,
            }
        } else {
            pc::SandboxObservation::Terminal {
                physical_incarnation: incarnation,
            }
        });
        assert!(
            ContainerEnvironmentProvider::adopt_environment_for_effect(
                &provider,
                ContainerEnvironmentAdoption::new(&desired, &handle),
                &fence,
                &authority,
            )
            .await
            .is_err(),
            "TC3 ordinary execution is refused"
        );
        let cleanup = ContainerEnvironmentProvider::prepare_terminal_environment_for_effect(
            &provider,
            &desired,
            Some(&handle),
            None,
            &fence,
            &authority,
        )
        .await
        .expect("TC1 exact terminal owner survives unavailable live control")
        .expect("TC1 physical participant remains");
        assert_eq!(
            cleanup
                .handle()
                .container_payload()
                .unwrap()
                .sandbox_control_incarnation,
            original_control,
            "TC1 unchanged immutable control identity"
        );
        assert!(
            runtime.st.lock().unwrap().control_binding_calls.is_empty(),
            "TC1 zero live control bind"
        );
        runtime.set_observation(pc::SandboxObservation::Ready);
        assert!(
            ContainerEnvironmentProvider::prepare_terminal_environment_for_effect(
                &provider,
                &desired,
                Some(&handle),
                None,
                &fence,
                &authority,
            )
            .await
            .is_err(),
            "TC2 Ready cannot fake a missing live binding"
        );
    }
}

#[tokio::test]
async fn disposing_terminal_preparation_reconstructs_only_physical_cleanup() {
    /* Disposing participant table CN2. Causes: C1 the exact durable handle
     * carries writable Secret and native Copy-Memory evidence; C2 the aggregate
     * has already durably accepted source preparation and the provider now
     * observes its physical cleanup gate as Disposing; C3 the same aggregate
     * terminal fence resumes cleanup. Effects: E1 reconstruction contains only
     * the existing physical owner; E2 disposal performs zero Pod exec/read,
     * RunV1 teardown, or Secret rewrite; E3 only the exact physical realization
     * is removed. Disposing itself is physical evidence, not a second durable
     * preparation receipt. Ordinary Terminal/Ready preparation is covered by
     * CN1; absence returns no owner in CT1.
     *
     * | Rule | observation | participants | live read/write | removal | Effect |
     * | C1 | Ready/Terminal | reconstructed | permitted | after durable I/O | CN1 |
     * | C2 | Disposing | physical only | forbidden | exact | E1+E2+E3 |
     * | C3 | absent | none | forbidden | none | CT1 | */
    let runtime = Arc::new(
        FakeRuntime::default()
            .with_native_memory_archive(one_file_archive("changed.txt", b"must-not-be-read"))
            .with_live_credential(b"must-not-be-written-back"),
    );
    let mut sandbox_spec = writable_credential_spec("disposing-physical-only");
    sandbox_spec.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "disposing-memory".into(),
            materialization_reference: Some("run-v1-must-not-be-reused".into()),
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/workspace/.mnt/memory".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = b"initial-credential".to_vec();
    let torn_down = Arc::new(AtomicBool::new(false));
    let provider = provider_without_broker(runtime.clone()).with_secret_broker(broker.clone());
    provider.install_memory_mounter(Arc::new(FakeMemoryMounter {
        torn_down: torn_down.clone(),
        fail_teardown: false,
    }));
    let created = provider
        .create_container(&sandbox_spec)
        .await
        .expect("CN2 create exact native source");
    let handle = pc::Sandbox::handle(&created);
    drop(created);
    let terminal = pc::SandboxEffectFence::new(
        "disposing-physical-cleanup",
        "worker-owner",
        "worker-incarnation",
        13,
        u64::MAX,
    )
    .unwrap();
    runtime.set_observation(pc::SandboxObservation::Disposing {
        physical_incarnation: handle.container_physical_incarnation().unwrap().to_string(),
    });

    let cleanup = ContainerEnvironmentProvider::prepare_terminal_environment_for_effect(
        &provider,
        &sandbox_spec,
        Some(&handle),
        None,
        &terminal,
        &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
    )
    .await
    .expect("CN2 prepare exact disposing owner")
    .expect("CN2 disposing realization still needs physical cleanup");
    cleanup
        .dispose_for_effect(
            &disposal_authorization(&terminal),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&terminal).effect_fence(),
            ),
        )
        .await
        .expect("CN2 resume exact physical cleanup");

    let state = runtime.st.lock().unwrap();
    assert!(state.spawned.is_empty(), "CN2/E2 zero Pod exec/read");
    assert_eq!(
        state.alive.get("cid-disposing-physical-only"),
        Some(&false),
        "CN2/E3"
    );
    drop(state);
    assert!(broker.writes.lock().unwrap().is_empty(), "CN2/E2");
    assert!(!torn_down.load(Ordering::SeqCst), "CN2/E2");
}
