//! Claim-fenced Memory snapshot/write-back tests over real HTTP.

mod support;

use std::sync::Arc;

use awaken_memory_store::{MemErr, MemoryRepository as _};
use awaken_resource_contract::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, ResourceAccess,
    ResourceCatalog as _, ResourceState,
};
use awaken_run_ingress::{DispatchQueue as _, MemoryDispatchStore, RunClaim, RunDispatch};
use awaken_runtime_host::{
    HttpMemoryRepository, WorkerMemoryService, memory_materialization_reference,
    worker_memory_router,
};
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

fn memory_input(
    binding: &str,
    store: &str,
    access: ResourceAccess,
) -> awaken_session_contract::ResolvedInput {
    awaken_session_contract::ResolvedInput {
        binding_id: awaken_resource_contract::BindingId::new(binding),
        source: awaken_session_contract::ResolvedInputSource::MemoryStore {
            memory_store_id: store.into(),
            config: MemoryStoreConfigVersion {
                memory_store_id: store.into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        },
        mount_path: format!("/memory/{binding}"),
        access,
        instructions: None,
    }
}

fn create_store(catalog: &awaken_resource_store::SqliteResourceStore, id: &str) {
    catalog
        .create_memory_store(
            MemoryStoreDefinition {
                id: id.into(),
                workspace_id: "workspace-memory".into(),
                name: id.into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            MemoryStoreConfigVersion {
                memory_store_id: id.into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        )
        .unwrap();
}

/// Cause/effect decision table:
/// | Rule | identity/claim | frozen binding | access | CAS base | Effect |
/// |---|---|---|---|---|---|
/// | M1 | current/live | exact store/config | read-write | n/a | return one atomic snapshot |
/// | M2 | current/live | exact store/config | read-write | current | create/update canonical repository |
/// | M3 | current/live | exact store/config | read-write | stale | typed conflict; preserve durable head |
/// | M4 | current/live | exact store/config | read-only | n/a | deny write before repository mutation |
/// | M5 | current/live | wrong Workspace | any | n/a | deny before repository access |
/// | M6 | current/stale | exact store/config | any | n/a | reject after claim settlement |
/// | M7 | current/live | exact store/config | read-write | n/a | reject rename outside the snapshot/CAS boundary |
/// | M8 | current/live | archived store | any | n/a | typed not-active denial; no repository read |
#[tokio::test]
async fn memory_snapshot_and_writeback_are_exact_claim_and_cas_fenced() {
    let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new());
    repository
        .create("memory-rw", "/seed.md", "seed")
        .await
        .unwrap();
    let catalog = Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
    create_store(&catalog, "memory-rw");
    create_store(&catalog, "memory-ro");
    let resources = awaken_session_contract::ResolvedSessionResources {
        inputs: vec![
            memory_input("rw", "memory-rw", ResourceAccess::ReadWrite),
            memory_input("ro", "memory-ro", ResourceAccess::ReadOnly),
        ],
        skills: Some(Vec::new()),
    };
    let dispatch = Arc::new(MemoryDispatchStore::new());
    dispatch
        .enqueue(
            RunDispatch::new(support::activation("memory"))
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from("workspace-memory"),
                ))
                .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
                    "workspace-memory",
                    serde_json::to_string(&resources).unwrap(),
                )),
        )
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-memory").await;
    let claimed = dispatch
        .claim(
            &identity.lease_owner(),
            60_000,
            support::unix_now_ms(),
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    let claim = RunClaim::from(&claimed.lease);
    let service = Arc::new(WorkerMemoryService::new(
        repository.clone(),
        catalog.clone(),
        dispatch.clone(),
        Arc::new(HeaderWorkerAuthenticator),
        directory,
    ));
    let address = support::serve(worker_memory_router(service)).await;
    let client = Arc::new(HttpMemoryRepository::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    ));
    let rw = memory_materialization_reference(
        "workspace-memory",
        "memory-rw",
        ConfigVersion::INITIAL,
        ResourceAccess::ReadWrite,
        &claim,
    )
    .unwrap();

    let snapshot = client.snapshot_heads(&rw).await.expect("M1");
    assert_eq!(snapshot.len(), 1, "M1");
    let created = client.create(&rw, "/new.md", "v1").await.expect("M2");
    let updated = client
        .update(&rw, &created.id, "v2", &created.content_sha256)
        .await
        .expect("M2");
    assert_eq!(updated.content.as_deref(), Some("v2"), "M2");

    let conflict = client
        .update(&rw, &created.id, "stale", &created.content_sha256)
        .await
        .expect_err("M3");
    assert!(matches!(conflict, MemErr::Conflict { .. }), "M3");
    assert_eq!(
        repository
            .get_by_path("memory-rw", "/new.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v2"),
        "M3"
    );

    let ro = memory_materialization_reference(
        "workspace-memory",
        "memory-ro",
        ConfigVersion::INITIAL,
        ResourceAccess::ReadOnly,
        &claim,
    )
    .unwrap();
    assert!(client.create(&ro, "/denied.md", "no").await.is_err(), "M4");
    assert!(
        repository
            .get_by_path("memory-ro", "/denied.md")
            .await
            .unwrap()
            .is_none(),
        "M4"
    );

    let wrong_workspace = memory_materialization_reference(
        "workspace-other",
        "memory-rw",
        ConfigVersion::INITIAL,
        ResourceAccess::ReadWrite,
        &claim,
    )
    .unwrap();
    assert!(client.snapshot_heads(&wrong_workspace).await.is_err(), "M5");

    assert!(
        client.rename(&rw, "/seed.md", "/renamed.md").await.is_err(),
        "M7"
    );

    catalog
        .set_memory_state("workspace-memory", "memory-rw", ResourceState::Archived)
        .unwrap();
    let archived = client.snapshot_heads(&rw).await.expect_err("M8");
    assert!(
        archived.to_string().contains("not active"),
        "M8: {archived}"
    );

    dispatch
        .settle(
            &claim.run_id,
            claim.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    assert!(client.snapshot_heads(&rw).await.is_err(), "M6");
}
