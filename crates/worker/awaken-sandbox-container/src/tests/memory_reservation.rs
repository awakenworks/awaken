//! Memory bind reservation and compensation at the container create boundary.

use super::*;

#[test]
fn reserved_memory_bind_exists_before_container_creation() {
    // Reservation decision table: C1 a copy-backed MemoryStore bind is needed;
    // C2 the runtime validates source paths during container creation; C3 the
    // MemoryMounter has not hydrated bytes yet. C1+C2+C3 => one existing empty
    // directory under the private staging guard, with the exact planned source
    // path. Dropping an uncommitted guard removes it. Without C1, no Memory
    // directory is reserved; native-volume backends own their own projection.
    let mut spec = spec("memory-reservation-before-create");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "store-42".into(),
            materialization_reference: Some("effect-reference-42".into()),
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/mnt/memory/notes".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let mut plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();
    let mut staging = crate::staged_mounts::StagedMounts::default();
    crate::staged_mounts::reserve_memory_binds(&spec, &mut plan, &mut staging, false).unwrap();
    let reserved = plan
        .binds
        .iter()
        .find(|bind| bind.mount_path == "/mnt/memory/notes")
        .expect("one exact Memory bind");
    let path = std::path::PathBuf::from(&reserved.source_ref);
    assert!(
        path.is_dir(),
        "Podman can validate the bind before hydration"
    );
    assert_eq!(
        std::fs::read_dir(&path).unwrap().count(),
        0,
        "no pre-hydration bytes"
    );
    drop(staging);
    assert!(
        !path.exists(),
        "uncommitted reservation is owned by the guard"
    );
    let mut native_plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();
    let mut native_staging = crate::staged_mounts::StagedMounts::default();
    crate::staged_mounts::reserve_memory_binds(&spec, &mut native_plan, &mut native_staging, true)
        .unwrap();
    assert!(
        native_staging.guard.is_none(),
        "native volume has no host staging root"
    );
    assert!(
        native_plan
            .binds
            .iter()
            .all(|bind| bind.mount_path != "/mnt/memory/notes"),
        "native Memory volume does not add a host bind"
    );
}

#[tokio::test]
async fn create_failure_compensates_only_when_the_runtime_proves_no_backend_effect() {
    /* Reservation-first cause/effect table CR1. Causes: C1 physical prepare
     * proves no backend effect or loses its response; C2 the plan reserved an
     * exact host Memory path; C3 Memory materialization has not begun. Effects:
     * R1 C1(definite)+C2+C3 removes staging with zero Memory teardown; R2
     * C1(unknown)+C2+C3 retains the backend-bound root and its empty bind
     * source, propagates the typed error, and still performs zero Memory
     * mount/teardown. The old
     * Memory-before-create/process-ledger path is deliberately absent. */
    fn memory_spec(scope: &str) -> pc::SandboxSpec {
        let mut requested = spec(scope);
        requested.mounts.push(pc::MountRequirement {
            mount_id: "notes".into(),
            source: pc::MountSource::MemoryStore {
                store_id: "store-create-outcome".into(),
                materialization_reference: None,
                write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
            },
            mount_path: "/workspace/.mnt/notes".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Session,
            required: true,
        });
        requested
    }

    let definite_runtime = Arc::new(FakeRuntime {
        st: Mutex::new(FakeState {
            fail_create: true,
            ..Default::default()
        }),
    });
    let definite_teardown = Arc::new(AtomicBool::new(false));
    let definite_provider = provider(definite_runtime.clone());
    definite_provider.install_memory_mounter(Arc::new(FakeMemoryMounter {
        torn_down: definite_teardown.clone(),
        fail_teardown: false,
    }));
    assert!(
        definite_provider
            .create_container(&memory_spec("definite-create-rejection"))
            .await
            .is_err(),
        "D1 runtime rejection propagates"
    );
    assert!(!definite_teardown.load(Ordering::SeqCst), "R1 zero mount");
    let definite_source = definite_runtime
        .st
        .lock()
        .unwrap()
        .attempted_binds
        .iter()
        .find(|bind| bind.mount_path == "/workspace/.mnt/notes")
        .unwrap()
        .source_ref
        .clone();
    assert!(
        !std::path::Path::new(&definite_source).exists(),
        "R1 definite rejection drops host staging"
    );

    let ambiguous_runtime = Arc::new(FakeRuntime {
        st: Mutex::new(FakeState {
            may_have_committed_create: true,
            ..Default::default()
        }),
    });
    let ambiguous_teardown = Arc::new(AtomicBool::new(false));
    let ambiguous_provider = provider(ambiguous_runtime.clone());
    ambiguous_provider.install_memory_mounter(Arc::new(FakeMemoryMounter {
        torn_down: ambiguous_teardown.clone(),
        fail_teardown: false,
    }));
    let error = match ambiguous_provider
        .create_container(&memory_spec("ambiguous-create-response"))
        .await
    {
        Ok(_) => panic!("M1 response loss cannot publish a Sandbox"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("may have committed"), "E2");
    assert!(
        !ambiguous_teardown.load(Ordering::SeqCst),
        "R2 zero Memory mount"
    );
    let ambiguous_source = ambiguous_runtime
        .st
        .lock()
        .unwrap()
        .attempted_binds
        .iter()
        .find(|bind| bind.mount_path == "/workspace/.mnt/notes")
        .unwrap()
        .source_ref
        .clone();
    let ambiguous_memory = std::path::Path::new(&ambiguous_source);
    assert!(
        ambiguous_memory.is_dir(),
        "R2 reserved bind path is retained"
    );
    assert_eq!(
        std::fs::read_dir(ambiguous_memory).unwrap().count(),
        0,
        "R2 Memory materialization did not start"
    );
    // The test owns no backend object, so it may clean only its injected stage
    // after observing the retention boundary. Production has no such proof and
    // intentionally retains it.
    let retained_root = std::path::Path::new(&ambiguous_source)
        .parent()
        .expect("staged file parent");
    assert!(retained_root.exists(), "R2 backend-bound root retained");
    std::fs::remove_dir_all(retained_root).expect("test-only retained stage cleanup");
}

#[tokio::test]
async fn activation_rejection_retains_original_memory_only_after_an_uncertain_effect() {
    /* Activation/Memory cause-effect table. This is the consumer half of the
     * real-Kubernetes HTTP regressions: that adapter produces the existing
     * after_mutation class for a changed scheduler refusal, missing RV, or
     * DELETE CAS 409. Here the same original FakeRuntime activation edge
     * returns those port outcomes after actual provider reservation + Memory
     * mount + pre-activation receipt; no test constructs a realization context.
     * C1 failure before activation effect / post-effect uncertainty; C2 original
     * Copy handle mounted. M1 before-effect Backend+C2 -> teardown=1. M2-M5
     * post-effect state/RV/CAS/already-uncertain+C2 -> teardown=0, exact original
     * backend attempt/fence/staging/receipt retained, then the original recovery
     * path reuses them without remount, reactivation, or a second identity.
     * ClaimedEffect itself completes Err and settles by its unchanged T2 law;
     * these retained owners are backend responsibility, not a live RAII claim.
     * Existing CR1 still covers rejection before any mount. This table proves
     * real provider/Memory I/O over its original fake, not a physical cluster.
     */
    for (row, failure) in [
        (
            "before-effect",
            ActivationFailure::BeforeEffect(RuntimeError::Backend(
                "activation rejected before effect".into(),
            )),
        ),
        (
            "scheduler-state",
            ActivationFailure::AfterEffect(
                RuntimeError::Conflict("scheduler refusal changed".into()).after_mutation(),
            ),
        ),
        (
            "missing-rv",
            ActivationFailure::AfterEffect(
                RuntimeError::Backend("unscheduled Pod has no valid resourceVersion".into())
                    .after_mutation(),
            ),
        ),
        (
            "delete-cas",
            ActivationFailure::AfterEffect(
                RuntimeError::Conflict("stale DELETE UID or resourceVersion".into())
                    .after_mutation(),
            ),
        ),
        (
            "already-uncertain",
            ActivationFailure::AfterEffect(
                RuntimeError::MayHaveCommitted("original ambiguous activation".into())
                    .after_mutation(),
            ),
        ),
    ] {
        let after_effect = row != "before-effect";
        let scope = format!("activation-rejection-{row}");
        let cid = format!("cid-{scope}");
        let runtime = Arc::new(FakeRuntime {
            st: Mutex::new(FakeState {
                recover_prepared_reservations: true,
                host_live_input_bind: true,
                activation_failure: Some(failure),
                ..Default::default()
            }),
        });
        let requested = activation_memory_spec(&scope);
        let mount_calls = Arc::new(AtomicUsize::new(0));
        let recovery_calls = Arc::new(AtomicUsize::new(0));
        let teardown_calls = Arc::new(AtomicUsize::new(0));
        let mounter = Arc::new(OrderedCopyMounter {
            runtime: runtime.clone(),
            scope: scope.clone(),
            mount_calls: mount_calls.clone(),
            recovery_calls: recovery_calls.clone(),
            teardown_calls: teardown_calls.clone(),
        });
        let fence =
            pc::SandboxEffectFence::new("create", "worker", "boot", 1, u64::MAX - 1).unwrap();
        let authority = pc::test_support::EffectAuthority::new(&fence);
        let first = provider(runtime.clone());
        first.install_memory_mounter(mounter.clone());
        let pending = pc::SandboxProvider::claim_for_effect(
            &first,
            &requested,
            pc::SandboxEffect::Fenced {
                fence: &fence,
                authority: &authority,
            },
            pc::SandboxRealizationIntent::Create,
        );
        assert_eq!(
            pending.reconciler(),
            Some(crate::CONTAINER_REALIZATION_RECONCILER)
        );
        assert_eq!(
            mount_calls.load(Ordering::SeqCst),
            0,
            "M claim construction has no effect"
        );
        let error = match pending.await {
            Ok(_) => panic!("M failure cannot publish a Sandbox"),
            Err(error) => error,
        };
        assert_eq!(
            mount_calls.load(Ordering::SeqCst),
            1,
            "M original mount actually ran"
        );
        assert_eq!(recovery_calls.load(Ordering::SeqCst), 0, "M fresh source");
        assert_eq!(
            teardown_calls.load(Ordering::SeqCst),
            usize::from(!after_effect),
            "M1 compensates; M2-M5 retain actual handle"
        );
        if after_effect {
            assert!(
                matches!(&error, pc::SandboxError::Transport(_)),
                "M2-M5 {row}: {error}"
            );
            assert_eq!(
                error
                    .to_string()
                    .matches("effect may have committed")
                    .count(),
                1,
                "M idempotent post-mutation wrapper"
            );
        } else {
            assert!(
                matches!(&error, pc::SandboxError::Backend { .. }),
                "M1 definite failure retains original class"
            );
        }
        let (original_attempt, root) = {
            let state = runtime.st.lock().unwrap();
            assert_eq!(state.activation_calls, 1, "M original activation");
            assert_eq!(
                state.alive.get(&cid),
                Some(&true),
                "M original reservation remains"
            );
            assert_eq!(
                state.active_reservations.contains(&scope),
                after_effect,
                "M actual fake effect boundary"
            );
            assert!(
                state.removals.is_empty(),
                "M no fabricated disposal/receipt"
            );
            assert_eq!(
                state.effect_fences.as_slice(),
                std::slice::from_ref(&fence),
                "M same original grant"
            );
            (
                state.prepared_attempts[&scope].clone(),
                state.recovered_staging.clone().unwrap(),
            )
        };
        assert!(
            !original_attempt.is_empty(),
            "M provider minted original attempt"
        );
        drop(first);
        assert!(
            root.is_dir(),
            "M backend-bound staging outlives failed caller"
        );
        assert!(
            root.join(crate::staged_mounts::MEMORY_PRE_ACTIVATION_RECEIPT)
                .is_file(),
            "M original receipt survives"
        );
        if after_effect {
            let second = provider(runtime.clone());
            second.install_memory_mounter(mounter.clone());
            let recovered = pc::SandboxProvider::claim_for_effect(
                &second,
                &requested,
                pc::SandboxEffect::Fenced {
                    fence: &fence,
                    authority: &authority,
                },
                pc::SandboxRealizationIntent::Create,
            )
            .await
            .expect("M original active reservation recovers");
            assert!(recovered.is_recovered(), "M original recovery disposition");
            assert_eq!(mount_calls.load(Ordering::SeqCst), 1, "M no remount");
            assert_eq!(
                recovery_calls.load(Ordering::SeqCst),
                0,
                "M no compensation"
            );
            assert_eq!(
                teardown_calls.load(Ordering::SeqCst),
                0,
                "M retained handle not torn down"
            );
            let state = runtime.st.lock().unwrap();
            assert_eq!(
                state.prepared_attempts[&scope], original_attempt,
                "M original backend responsibility identity"
            );
            assert_eq!(
                state.recovered_staging.as_ref(),
                Some(&root),
                "M exact original staging"
            );
            assert_eq!(state.activation_calls, 1, "M no second activation");
            assert_eq!(state.alive.len(), 1, "M no second physical owner");
            assert!(state.removals.is_empty());
            assert!(
                state
                    .effect_fences
                    .iter()
                    .all(|observed| observed == &fence)
            );
            drop(state);
            drop(recovered);
            drop(second);
        }
        drop(mounter);
        drop(runtime);
        // This test owns only a fake reservation and this exact temporary Copy
        // directory. Removing its bytes after the assertions is fixture hygiene,
        // never production Pod disposal or Session cleanup evidence.
        std::fs::remove_dir_all(root).expect("test-only retained Copy staging cleanup");
    }
}
