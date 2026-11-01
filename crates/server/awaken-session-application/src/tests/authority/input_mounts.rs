//! Both application callers consume the one definition/mount normalization.
use super::*;
use crate::{SessionInputRequest, SessionInputTarget};
use awaken_resource_contract::{
    BindingId, ConfigVersion, FileId, InputBinding, InputResourceId, MemoryStoreConfigVersion,
    MemoryStoreDefinition, RegisterMemoryStore, ResourceAccess, ResourceRegistryError,
    ResourceState, RetentionPolicy,
};

async fn memory_registry() -> Arc<FaultInjectingResourceRegistry> {
    let inner = crate::tests::creation::ephemeral_resource_registry().await;
    for id in ["mem-a", "mem-b"] {
        inner
            .register_memory_store(RegisterMemoryStore {
                definition: MemoryStoreDefinition {
                    id: id.into(),
                    workspace_id: "workspace".into(),
                    name: "Project Memory".into(),
                    description: String::new(),
                    metadata: Default::default(),
                    state: ResourceState::Active,
                    current_config_version: ConfigVersion::INITIAL,
                    timestamps: Default::default(),
                },
                initial_config: MemoryStoreConfigVersion {
                    memory_store_id: id.into(),
                    version: ConfigVersion::INITIAL,
                    retention_policy: RetentionPolicy::default(),
                },
            })
            .await
            .unwrap();
    }
    Arc::new(FaultInjectingResourceRegistry::new(inner))
}

fn derived(id: &str) -> SessionInputRequest {
    SessionInputRequest::derived_memory_store(id.into(), ResourceAccess::ReadWrite, None)
}

fn explicit_file(path: &str) -> SessionInputRequest {
    SessionInputRequest {
        target: SessionInputTarget::File(FileId::from("file-explicit")),
        mount_path: path.into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        derived_memory_mount: false,
    }
}

fn mounts(requests: &[SessionInputRequest]) -> Vec<&str> {
    requests
        .iter()
        .map(|request| request.mount_path.as_str())
        .collect()
}

fn assert_run_error(actual: RunError, expected: RunError, rule: &str) {
    assert_eq!(
        (actual.kind, actual.code, actual.message),
        (expected.kind, expected.code, expected.message),
        "{rule}"
    );
}

#[tokio::test]
async fn input_mount_normalization_preserves_absent_catalogue_and_lookup_failures() {
    // Cause/effect N1-N2: no requests/all-explicit => no Inventory needed or
    // observed, even when selected Inventory has a queued fault. A derived
    // request => absent/missing/foreign/error is the ORIGINAL BadRequest, not
    // the later resolver's Unavailable/Internal projection. A good prefix
    // followed by missing definition retains exact ordered reads, with no
    // Repository/Vault participant. Registration/activation use the original
    // fault-only decorator over the real Registry; no second catalogue exists.
    use crate::session_input::normalize_session_input_mounts as normalize;
    let repository = SessionInputRequest {
        target: SessionInputTarget::Repository {
            remote_url: "https://example.test/source.git".into(),
            credential_material: None,
            initial_branch: None,
            initial_commit: None,
        },
        mount_path: "/workspace/source".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        derived_memory_mount: false,
    };
    for requests in [
        Vec::new(),
        vec![explicit_file("/workspace/input"), repository],
    ] {
        let normalized = normalize(None, "workspace", &requests, &[]).await.unwrap();
        assert_eq!(
            mounts(&normalized),
            mounts(&requests),
            "N1 absent Inventory"
        );
    }
    let registry = memory_registry().await;
    let inventory = Some(registry.as_ref() as &dyn awaken_resource_contract::ResourceInventory);
    let unavailable = ResourceRegistryError::AuthorityUnavailable {
        detail: "selected fault".into(),
    };
    *registry.memory_read_failure.lock().unwrap() = Some(unavailable.clone());
    normalize(
        inventory,
        "workspace",
        &[explicit_file("/workspace/input")],
        &[],
    )
    .await
    .unwrap();
    assert!(
        registry.memory_reads.lock().unwrap().is_empty(),
        "N1 selected but never read"
    );
    assert_eq!(
        *registry.memory_read_failure.lock().unwrap(),
        Some(unavailable.clone()),
        "N1 fault unconsumed"
    );
    assert_run_error(
        normalize(inventory, "workspace", &[derived("mem-a")], &[])
            .await
            .unwrap_err(),
        RunError::bad_request(unavailable.to_string()),
        "N2 original authority error projection",
    );
    assert_run_error(
        normalize(None, "workspace", &[derived("mem-a")], &[])
            .await
            .unwrap_err(),
        RunError::bad_request("memory resources require a configured Resource Registry"),
        "N2 absent",
    );
    for (scope, id) in [("workspace", "missing"), ("foreign", "mem-a")] {
        assert_run_error(
            normalize(inventory, scope, &[derived(id)], &[])
                .await
                .unwrap_err(),
            RunError::bad_request(format!("MemoryStore `{id}` not found in this workspace")),
            "N2 scoped absence",
        );
    }
    for error in [
        ResourceRegistryError::Indeterminate {
            detail: "unknown".into(),
        },
        ResourceRegistryError::CorruptData("corrupt".into()),
    ] {
        *registry.memory_read_failure.lock().unwrap() = Some(error.clone());
        assert_run_error(
            normalize(inventory, "workspace", &[derived("mem-a")], &[])
                .await
                .unwrap_err(),
            RunError::bad_request(error.to_string()),
            "N2 all original Inventory errors remain BadRequest",
        );
    }
    registry.memory_reads.lock().unwrap().clear();
    assert!(
        normalize(
            inventory,
            "workspace",
            &[derived("mem-a"), derived("missing")],
            &[]
        )
        .await
        .is_err()
    );
    assert_eq!(
        *registry.memory_reads.lock().unwrap(),
        vec![
            ("workspace".into(), "mem-a".into()),
            ("workspace".into(), "missing".into())
        ],
        "N2 ordered prefix"
    );
    assert_eq!(registry.registration_calls.load(Ordering::SeqCst), 0);
    assert_eq!(registry.activation_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn input_mount_normalization_is_shared_by_creation_and_manifest_roles() {
    // N3 builds the selected Inventory through the existing consuming Builder;
    // no late fixture setter supplies the compilation capability.
    // N3/N5: explicit paths (including a LATER request) and Agent defaults
    // reserve every slot before definition reads. The same slug then selects
    // stable id and numeric suffixes in request order. Actual App preparation
    // and the existing manifest role must observe the SAME normalized paths;
    // neither configures a participant. N6 retained Inputs still compiles from
    // the original Inventory after App drop; it holds no App back-reference.
    // A typed duplicate is deliberately
    // rejected only by the original effective-bindings check AFTER lookup.
    let registry = memory_registry().await;
    let sessions = authority_repository("input mount Session store").await;
    let app = crate::tests::application_fixture::configured_application(
        Arc::new(NoopRuntime),
        crate::SessionPersistence::new(sessions, system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        Default::default(),
        Some(crate::SessionResources::catalog(registry.clone())),
        {
            let probe = Arc::new(crate::ExecutionObservationProbe::default());
            crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe)
        },
    );
    let mut first = derived("mem-a");
    first.instructions = Some("retain exact instructions".into());
    let requests = [
        first,
        explicit_file("/mnt/memory/project-memory"),
        derived("mem-b"),
        derived("mem-b"),
    ];
    let defaults = [InputBinding {
        binding_id: BindingId::new("default-slot"),
        target: InputResourceId::File(FileId::from("default-file")),
        mount_path: "/mnt/memory/project-memory-mem-b".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
    }];
    let expected = [
        "/mnt/memory/project-memory-mem-a",
        "/mnt/memory/project-memory",
        "/mnt/memory/project-memory-mem-b-2",
        "/mnt/memory/project-memory-mem-b-3",
    ];
    let normalized = app
        .resource_manifests
        .lowering("mount-session", "workspace")
        .normalize(&requests, &defaults)
        .await
        .unwrap();
    assert_eq!(
        mounts(&normalized),
        expected,
        "N3 manifest role preserves all seeds"
    );
    assert_eq!(normalized[0].access, requests[0].access);
    assert_eq!(normalized[0].instructions, requests[0].instructions);
    let app = Arc::new(app);
    let application = Arc::downgrade(&app);
    let inputs = app.inputs();
    drop(app);
    assert!(application.upgrade().is_none(), "N6 App was released");
    let prepared = inputs
        .prepare_session_inputs("mount-session", "workspace", &requests, &defaults)
        .await
        .unwrap();
    let planned = prepared
        .effective_bindings()
        .iter()
        .take(requests.len())
        .map(|binding| binding.mount_path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(planned, expected, "N3 create uses same normalization");
    assert_eq!(
        prepared.effective_bindings()[0].binding_id.as_str(),
        "session:mount-session:input:0",
        "N3 original create identity"
    );
    registry.memory_reads.lock().unwrap().clear();
    let duplicates = [
        derived("mem-a"),
        explicit_file("/duplicate"),
        explicit_file("/duplicate"),
    ];
    let error = inputs
        .prepare_session_inputs("duplicates", "workspace", &duplicates, &[])
        .await
        .err()
        .expect("N5 duplicate typed mounts reject");
    assert_eq!(
        error.kind,
        awaken_session_types::RunErrorKind::BadRequest,
        "N5 typed collision"
    );
    assert_eq!(
        *registry.memory_reads.lock().unwrap(),
        vec![("workspace".into(), "mem-a".into())],
        "N5 lookup still precedes complete logical collision validation"
    );
    assert_eq!(registry.registration_calls.load(Ordering::SeqCst), 0);
    assert_eq!(registry.activation_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fixture_inventory_replacement_refuses_after_inputs_are_shared() {
    // Fixture decision table: N7 shared selected Inputs + Registry replacement
    // -> refuse BEFORE either the App's Registry or Inputs' Inventory changes.
    // N3 covers configuration before sharing. Both paths after refusal retain
    // the original absent-Inventory error and never enter the rejected Registry.
    let registry = memory_registry().await;
    let sessions = authority_repository("shared Inputs fixture store").await;
    let mut app = application(sessions, Arc::new(RecordingEnvironmentSource::default()));
    let inputs = app.inputs();
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.set_resource_registry(registry.clone());
    }));
    assert!(refused.is_err(), "N7 shared fixture refuses replacement");
    let requests = [derived("mem-a")];
    let selected = inputs
        .prepare_session_inputs("refused", "workspace", &requests, &[])
        .await
        .err()
        .expect("N7 selected Inventory remains absent");
    let manifest = app
        .resource_manifests
        .lowering("refused", "workspace")
        .normalize(&requests, &[])
        .await
        .expect_err("N7 Resource authority remains absent");
    let expected =
        || RunError::bad_request("memory resources require a configured Resource Registry");
    assert_run_error(selected, expected(), "N7 selected Inputs");
    assert_run_error(manifest, expected(), "N7 Resource boundary");
    assert!(registry.memory_reads.lock().unwrap().is_empty());
    assert_eq!(registry.registration_calls.load(Ordering::SeqCst), 0);
    assert_eq!(registry.activation_calls.load(Ordering::SeqCst), 0);
}
