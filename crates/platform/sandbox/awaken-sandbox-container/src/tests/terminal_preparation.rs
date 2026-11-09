//! Terminal preparation reconstructs only exact physical cleanup: no live control
//! binding, no ordinary execution, and no rebinding of a closed owner.
use super::*;
use awaken_provisioning_contract::SandboxAdoption;
use awaken_provisioning_contract::SandboxProvider;

#[tokio::test]
async fn terminal_cleanup_retains_original_rootfs_after_default_image_upgrade() {
    // Causes: C1 exact frozen spec and published original handle; C2 a rolling
    // upgrade changes only the provider's default image; C3 current terminal
    // effect authority; C4 ordinary recovery or terminal preparation. Effects:
    // I1 ordinary recovery refuses the changed execution configuration; I2
    // terminal preparation recovers the original source without another Create,
    // live Control binding or physical deletion. C5 original scope/spec/rootfs
    // evidence agrees. Decision rules: R1 C1+C2+C4
    // recovery -> I1; R2 C1+C2+C3+C4 terminal -> I2. The provider must validate
    // retained original projection evidence, never bypass its fingerprints.
    // R3 missing C3 or any !C5 -> incompatible before runtime observation,
    // with the original source retained and no create/control/delete effects.
    let runtime = Arc::new(
        FakeRuntime::default()
            .with_live_input_projection()
            .with_native_memory_archive(Vec::new()),
    );
    let mut original_provider = provider(runtime.clone());
    original_provider.default_image = "image:before-upgrade".into();
    let mut requested = spec("terminal-image-upgrade");
    requested.environment = None;
    requested.mounts.clear();
    let original = original_provider
        .create_container(&requested)
        .await
        .unwrap();
    let handle = pc::Sandbox::handle(&original);
    let mut replacement = provider(runtime.clone());
    replacement.default_image = "image:after-upgrade".into();
    let fence =
        pc::SandboxEffectFence::new("terminal-image", "worker", "boot", 1, u64::MAX).unwrap();
    let ordinary = replacement
        .observe(
            SandboxAdoption::new(&requested, &handle),
            pc::SandboxObservationBasis::Unattached,
        )
        .await
        .unwrap();
    assert!(
        matches!(ordinary, pc::SandboxObservation::Incompatible { .. }),
        "R1"
    );
    for row in [
        "no-fence",
        "foreign-scope",
        "changed-spec",
        "changed-rootfs",
    ] {
        let mut candidate_spec = requested.clone();
        let candidate_handle = if row == "changed-rootfs" {
            let mut wire = serde_json::to_value(&handle).unwrap();
            wire["payload"]["creation_rootfs"] =
                serde_json::to_value(crate::RootfsPlan::Image("tampered-image".into())).unwrap();
            serde_json::from_value(wire).unwrap()
        } else {
            handle.clone()
        };
        if row == "foreign-scope" {
            candidate_spec.scope = "foreign-session".into();
        }
        if row == "changed-spec" {
            candidate_spec.outputs_path = "/foreign-outputs".into();
        }
        let mut adoption = SandboxAdoption::new(&candidate_spec, &candidate_handle);
        adoption.observation_purpose = pc::SandboxObservationPurpose::TerminalCleanup;
        let basis = if row == "no-fence" {
            pc::SandboxObservationBasis::Unattached
        } else {
            adoption.effect_basis(&fence)
        };
        let before = runtime.st.lock().unwrap().observations;
        assert!(
            matches!(
                replacement.observe(adoption, basis).await.unwrap(),
                pc::SandboxObservation::Incompatible { .. }
            ),
            "R3 {row}"
        );
        assert_eq!(
            runtime.st.lock().unwrap().observations,
            before,
            "R3 before physical observation {row}"
        );
    }
    let prepared = replacement
        .prepare_terminal_for_effect(
            &requested,
            Some(&handle),
            None,
            &fence,
            &pc::test_support::EffectAuthority::new(&fence),
        )
        .await
        .expect("R2 terminal cleanup must retain the original image projection")
        .expect("R2 original physical source still exists");
    assert_eq!(prepared.handle().sandbox_id, requested.scope);
    let observed = runtime.st.lock().unwrap();
    assert_eq!(
        observed.created_command.len(),
        1,
        "R2 no replacement Create"
    );
    assert!(
        observed.control_binding_calls.is_empty(),
        "R2 no live Control"
    );
    assert!(
        observed.removals.is_empty(),
        "R2 preparation is not disposal"
    );
}

#[tokio::test]
async fn retirement_observes_original_identity_across_execution_configuration_changes() {
    // Cause/effect table RO: C1 purpose=Retirement(unprepared/prepared), Recovery
    // or TerminalCleanup; C2 current runtime/resident configuration exact/drifted;
    // C3 original effect fence present/absent; C4 full durable handle/legacy;
    // C5 Session scope exact/foreign; C6 runtime reports retained/incompatible.
    // R1 Retirement+C3+full+C5 reaches the ONE runtime observer irrespective of
    // C2, and preserves its C6 result. R2 missing C3/C4/C5 rejects before I/O.
    // R3 Recovery/TerminalCleanup keep strict current-configuration admission.
    // Every row is read-only: no create, exec, staging, control bind or removal.
    for purpose in [
        pc::SandboxObservationPurpose::WorkerRetirement {
            source_prepared: false,
        },
        pc::SandboxObservationPurpose::WorkerRetirement {
            source_prepared: true,
        },
        pc::SandboxObservationPurpose::Recovery,
        pc::SandboxObservationPurpose::TerminalCleanup,
    ] {
        for drift in [true, false] {
            let runtime = Arc::new(FakeRuntime::default());
            let provider = provider(runtime.clone());
            let mut requested = spec("retirement-configuration");
            requested.mounts.clear();
            let original = provider.create_container(&requested).await.unwrap();
            let handle = pc::Sandbox::handle(&original);
            let legacy =
                pc::SandboxHandle::container(&requested.scope, handle.container_payload().unwrap());
            let foreign = pc::SandboxHandle::container_v2(
                "foreign-session",
                crate::ContainerSandboxHandleV2 {
                    creation_rootfs: None,
                    previous: handle.container_payload().unwrap(),
                    adoption_fingerprint: handle.container_adoption_fingerprint().unwrap().unwrap(),
                    realization_fingerprint: handle.realization_fingerprint().unwrap().clone(),
                    owned_paths: handle.owned_paths().unwrap().to_vec(),
                },
            );
            if drift {
                runtime.st.lock().unwrap().configuration = Some(std::collections::BTreeMap::from(
                    [("current-installation".into(), "new".into())],
                ));
            }
            let fence =
                pc::SandboxEffectFence::new("original-create", "owner", "boot", 1, u64::MAX)
                    .unwrap();
            let retirement = matches!(
                purpose,
                pc::SandboxObservationPurpose::WorkerRetirement { .. }
            );
            for (source, fenced, exact_identity) in [
                (&handle, true, true),
                (&handle, false, false),
                (&legacy, true, false),
                (&foreign, true, false),
            ] {
                // Non-retirement compatibility is already owned by the existing
                // adoption matrix; this table adds only its exact-handle rows.
                if !retirement && !std::ptr::eq(source, &handle) {
                    continue;
                }
                for retained in [true, false] {
                    let observed = if retained {
                        pc::SandboxObservation::RetainedOriginal {
                            physical_incarnation: "original-uid".into(),
                        }
                    } else {
                        pc::SandboxObservation::Incompatible {
                            reason: "original source is foreign".into(),
                        }
                    };
                    runtime.set_observation(observed.clone());
                    let before = runtime.st.lock().unwrap().observations;
                    let mut adoption = SandboxAdoption::new(&requested, source);
                    adoption.observation_purpose = purpose;
                    let basis = if fenced {
                        adoption.effect_basis(&fence)
                    } else {
                        pc::SandboxObservationBasis::Unattached
                    };
                    let actual = provider.observe(adoption, basis).await.unwrap();
                    let reaches_runtime = if retirement { exact_identity } else { !drift };
                    assert_eq!(
                        runtime.st.lock().unwrap().observations,
                        before + u32::from(reaches_runtime),
                        "RO {purpose:?} drift={drift} fenced={fenced} exact={exact_identity}"
                    );
                    if reaches_runtime {
                        assert_eq!(actual, observed, "RO R1 exact backend fact");
                    } else {
                        assert!(
                            matches!(actual, pc::SandboxObservation::Incompatible { .. }),
                            "RO R2/R3"
                        );
                    }
                }
            }
            let state = runtime.st.lock().unwrap();
            assert_eq!(state.created_command.len(), 1, "RO no second Create");
            assert!(state.spawned.is_empty() && state.removals.is_empty());
            assert!(state.control_binding_calls.is_empty());
            assert_eq!(state.recovered_staging_calls, 0);
        }
    }
}

#[tokio::test]
async fn retained_original_observation_never_grants_adoption_or_terminal_preparation() {
    // Cause/effect table RA1: exact RetainedOriginal observation crosses the
    // legacy, current-unattached, current-fenced adoption and terminal-prepare
    // seams. Every row must actually observe, return Conflict, preserve the
    // original container and perform no staging, Control binding or deletion.
    // The observation's serialized round trip preserves its independent fact;
    // it is never reinterpreted as Ready, Terminal, Disposing or absence.
    for seam in ["legacy", "unattached", "fenced", "terminal"] {
        let runtime = Arc::new(
            FakeRuntime::default()
                .with_live_input_projection()
                .with_native_memory_archive(Vec::new()),
        );
        let provider = provider(runtime.clone());
        let mut desired = spec("retained-observation-admission");
        desired.mounts.clear();
        let original = provider.create_container(&desired).await.unwrap();
        let handle = pc::Sandbox::handle(&original);
        let id = original.container_id.clone();
        let observed = pc::SandboxObservation::RetainedOriginal {
            physical_incarnation: id.clone(),
        };
        let wire = serde_json::to_value(&observed).unwrap();
        assert_eq!(wire["retained_original"]["physical_incarnation"], id);
        assert_eq!(
            serde_json::from_value::<pc::SandboxObservation>(wire).unwrap(),
            observed
        );
        runtime.set_observation(observed);
        let fence = pc::SandboxEffectFence::new("retention", "owner", "boot", 2, u64::MAX).unwrap();
        let authority = pc::test_support::EffectAuthority::new(&fence);
        let observations = runtime.st.lock().unwrap().observations;
        let result = match seam {
            "legacy" => provider
                .adopt_container(&pc::SandboxHandle::container(
                    &desired.scope,
                    handle.container_payload().unwrap(),
                ))
                .await
                .map(drop),
            "unattached" => provider
                .adopt_for_effect(
                    SandboxAdoption::new(&desired, &handle),
                    pc::SandboxEffect::Unattached,
                )
                .await
                .map(drop),
            "fenced" => provider
                .adopt_for_effect(
                    SandboxAdoption::new(&desired, &handle),
                    pc::SandboxEffect::Fenced {
                        fence: &fence,
                        authority: &authority,
                    },
                )
                .await
                .map(drop),
            _ => provider
                .prepare_terminal_for_effect(&desired, Some(&handle), None, &fence, &authority)
                .await
                .map(drop),
        };
        assert!(
            matches!(result, Err(pc::SandboxError::Conflict(_))),
            "RA1 {seam}: {result:?}"
        );
        let state = runtime.st.lock().unwrap();
        assert_eq!(
            state.observations,
            observations + 1,
            "RA1 {seam} consumes the fact"
        );
        assert!(state.alive[&id]);
        assert!(state.removals.is_empty());
        assert!(state.control_binding_calls.is_empty());
        assert_eq!(state.recovered_staging_calls, 0);
        assert_eq!(state.created_command.len(), 1);
    }
}

#[tokio::test]
async fn terminal_control_reconstruction_does_not_require_a_live_binding() {
    // Control reconstruction table TC1: exact frozen spec/handle/fence plus a
    // Ready, Terminal or Disposing observation -> preserve the recorded control
    // incarnation for exact physical cleanup, with zero live binding calls.
    // TC2 ordinary Ready adoption still needs a real binding; absence rejects. TC3
    // ordinary adoption of Terminal remains refused. This provider fixture
    // asserts routing/identity/absence of live control, not a real Pod receipt.
    // TC4: terminal wrapper's existing publication slot is absorbing-closed;
    // an attempted publication must fail before any channel opens. Ordinary
    // Ready adoption still requires its real binding (TC2), never this closure.
    // TC5 Ready terminal preparation has the same closed publication slot and
    // zero live binding as Terminal. A new Worker's control artifact is not
    // authority to rebind the retained original during cleanup.
    for phase in ["ready", "terminal", "disposing"] {
        let disposing = phase == "disposing";
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
        runtime.set_observation(if phase == "ready" {
            pc::SandboxObservation::Ready
        } else if disposing {
            pc::SandboxObservation::Disposing {
                physical_incarnation: incarnation,
            }
        } else {
            pc::SandboxObservation::Terminal {
                physical_incarnation: incarnation,
            }
        });
        assert!(
            SandboxProvider::adopt_for_effect(
                &provider,
                SandboxAdoption::new(&desired, &handle),
                awaken_provisioning_contract::SandboxEffect::Fenced {
                    fence: &fence,
                    authority: &authority
                }
            )
            .await
            .is_err(),
            "TC2/TC3 ordinary execution requires live binding or is refused"
        );
        runtime.st.lock().unwrap().control_binding_calls.clear();
        let cleanup = SandboxProvider::prepare_terminal_for_effect(
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
        assert!(
            SandboxControlServicePublisher::publish_sandbox_control_service(
                cleanup.as_ref(),
                SandboxControlServiceKind::RepositoryGitCredential,
                Arc::new(UnavailableControlService),
            )
            .await
            .is_err(),
            "TC4 terminal Control cannot publish",
        );
        assert_eq!(
            runtime.st.lock().unwrap().control_channel_opens,
            0,
            "TC4 zero channel opens"
        );
        runtime.set_observation(pc::SandboxObservation::Ready);
        assert!(
            SandboxProvider::adopt_for_effect(
                &provider,
                SandboxAdoption::new(&desired, &handle),
                pc::SandboxEffect::Fenced {
                    fence: &fence,
                    authority: &authority
                },
            )
            .await
            .is_err(),
            "TC2 ordinary Ready adoption cannot fake a missing live binding"
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

    let cleanup = SandboxProvider::prepare_terminal_for_effect(
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
