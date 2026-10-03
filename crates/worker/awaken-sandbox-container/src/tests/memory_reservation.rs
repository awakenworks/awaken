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
