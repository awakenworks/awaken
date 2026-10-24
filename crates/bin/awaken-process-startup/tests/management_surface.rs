//! The composed product plane serves the canonical Worker readiness projection.

use awaken_process_startup::build_ephemeral_all_in_one_router as build_all_in_one_router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_product_plane_can_read_worker_fleet_readiness() {
    // The fleet projection is only useful if it is reachable where a product
    // plane actually asks. Its unit tests prove the decision; this proves the
    // composed server answers the route at all, which is the failure this
    // surface had before: the Coordinator served no `/v1/workers/readiness`,
    // so Flow's hosted fleet adapter received a permanent 404 and reported the
    // work store as unavailable.
    let app = build_all_in_one_router(std::sync::Arc::new(awaken_cli::LocalExecution)).await;

    let (status, body) = call(
        &app,
        "GET",
        "/v1/workers/readiness?capabilities=coding",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["ready"],
        json!(false),
        "this composition registers no Worker"
    );
    assert_eq!(body["total_workers"], json!(0));
    assert_eq!(
        body["missing_capabilities"],
        json!(["coding"]),
        "an empty fleet offers nothing, so the requirement is named"
    );

    // The weaker question is a different route answer, not an error.
    let (status, body) = call(&app, "GET", "/v1/workers/readiness", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["missing_capabilities"], json!([]));
}

// Cause/effect matrix: either public router chooses AppA's catalog/content;
// a physically separate AppB has the same opaque store and a distinct sentinel path. HTTP
// writes and the real AppA reclamation fence must affect only AppA. Embedded
// persistence deliberately stores catalog and Memory in different databases.
#[tokio::test]
async fn memory_store_http_reclamation_uses_the_selected_application_content() {
    assert_selected_memory_content(|application, _sentinel| {
        awaken_protocol_managed::memory_stores_router(application)
    })
    .await;
}

#[tokio::test]
async fn aggregate_resource_http_reclamation_uses_the_selected_application_content() {
    assert_selected_memory_content(|application, _sentinel| {
        awaken_protocol_managed::resources_router(
            awaken_protocol_managed::ResourcesRouterInput::new(
                application,
                std::sync::Arc::new(
                    awaken_resource_contract::UnavailableRepositoryCredentialValidator,
                ),
            ),
        )
    })
    .await;
}

async fn assert_selected_memory_content(
    build_router: impl FnOnce(
        &awaken_resource_application::ResourcesApplication,
        &awaken_resource_application::ResourcesApplication,
    ) -> axum::Router,
) {
    use awaken_protocol_managed::types::memory::AuthenticatedMemoryActor;
    use awaken_resource_contract::{MemoryActor, MemoryCopyConflict};
    use std::sync::Arc;

    let directory = tempfile::tempdir().unwrap();
    let selected_root = directory.path().join("selected");
    let sentinel_root = directory.path().join("sentinel");
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_700_000_000_000));
    let selected = awaken_resource_persistence::open_embedded(
        &selected_root,
        clock.clone(),
        Arc::new(awaken_resource_contract::LocalInvalidator::default()),
    )
    .await
    .unwrap();
    let sentinel = awaken_resource_persistence::open_embedded(
        &sentinel_root,
        clock,
        Arc::new(awaken_resource_contract::LocalInvalidator::default()),
    )
    .await
    .unwrap();
    let catalog_path = selected_root.join("resources.db").canonicalize().unwrap();
    let content_path = selected_root.join("memory_fs.db").canonicalize().unwrap();
    assert_ne!(catalog_path, content_path, "catalog may live in another DB");
    let actor = MemoryActor::ApiActor {
        api_key_id: "selected-application-api".into(),
    };
    let router = build_router(&selected, &sentinel)
        .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
            "test".into(),
        )))
        .layer(axum::Extension(AuthenticatedMemoryActor(actor.clone())));
    let (status, body) = call(
        &router,
        "POST",
        "/v1/memory_stores",
        Some(json!({ "name": "selected-store" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let store = body["id"].as_str().unwrap();
    let a = selected.authorities();
    let b = sentinel.authorities();
    let sentinel_actor = MemoryActor::ServiceAccountActor {
        service_account_id: "other-application".into(),
    };
    b.memory_writer()
        .create_as(store, "/sentinel.md", "sentinel", Some(&sentinel_actor))
        .await
        .unwrap();
    let proposal = MemoryCopyConflict {
        path: "/same.md".into(),
        base: None,
        content: Some("retained losing proposal".into()),
    };
    b.memory_copy_conflict_preservation()
        .preserve_copy_conflict(store, &proposal)
        .await
        .unwrap();
    let sentinel_before = (
        b.memory_snapshot_source()
            .snapshot_heads(store)
            .await
            .unwrap(),
        b.memory_versions().list_versions(store).await.unwrap(),
        b.memory_copy_conflict_queries()
            .list_copy_conflicts(store)
            .await
            .unwrap(),
    );
    assert_eq!(sentinel_before.0[0].path, "/sentinel.md");
    assert_eq!(sentinel_before.0[0].content.as_deref(), Some("sentinel"));
    assert_eq!(sentinel_before.1[0].created_by, Some(sentinel_actor));
    assert_eq!(sentinel_before.2, vec![proposal.clone()]);
    let (status, created) = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories"),
        Some(json!({ "path": "/same.md", "content": "selected-original" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let memory_id = created["id"].as_str().unwrap();
    let (status, updated) = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories/{memory_id}"),
        Some(json!({
            "content": "selected-updated",
            "precondition": { "type": "content_sha256", "content_sha256": created["content_sha256"] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let heads = a
        .memory_snapshot_source()
        .snapshot_heads(store)
        .await
        .unwrap();
    assert_eq!(heads.len(), 1, "HTTP content belongs to selected AppA");
    assert_eq!(heads[0].id, memory_id);
    assert_eq!(heads[0].content.as_deref(), Some("selected-updated"));
    assert_eq!(heads[0].version, 2);
    assert_eq!(heads[0].content_sha256, updated["content_sha256"]);
    let versions = a.memory_versions().list_versions(store).await.unwrap();
    assert_eq!(versions.len(), 2);
    for response in [&created, &updated] {
        let version = versions
            .iter()
            .find(|version| version.id == response["memory_version_id"])
            .expect("the exact HTTP version is in AppA history");
        assert_eq!(version.memory_id, memory_id);
        assert_eq!(version.created_by, Some(actor.clone()));
    }
    a.memory_copy_conflict_preservation()
        .preserve_copy_conflict(store, &proposal)
        .await
        .unwrap();
    assert_memory_sentinel(&sentinel, store, &sentinel_before).await;
    reclaim_selected_memory(&router, &selected, store).await;
    assert_memory_sentinel(&sentinel, store, &sentinel_before).await;
}

async fn reclaim_selected_memory(
    router: &axum::Router,
    application: &awaken_resource_application::ResourcesApplication,
    store: &str,
) {
    use awaken_resource_contract::{ResourcePurgeEvidence, ResourcePurgeStatus, ResourceState};
    use awaken_time::WallClockExt as _;

    let (status, receipt) = call(
        router,
        "DELETE",
        &format!("/v1/memory_stores/{store}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["type"], "memory_store_deleted");
    assert_eq!(receipt["id"], store);
    let authority = application.authorities();
    let definition = authority
        .resource_registry()
        .find_memory_store("test", store)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(definition.state, ResourceState::Deleted);
    let repository = authority.reclamation();
    let clock = authority.clock();
    let intents = repository.recoverable(clock.now_ms(), 10).await.unwrap();
    assert_eq!(intents.len(), 1, "HTTP deletion schedules the real intent");
    assert_eq!(intents[0].target.resource_id, store);
    let reclaimer = awaken_resource_reclaimer::ResourceReclaimer::new(
        "selected-application-content",
        1_000,
        repository.clone(),
        application.physical_cleanup(),
    )
    .unwrap()
    .with_preparation(application.purge_preparation())
    .with_guard(application.lifecycle_guard());
    let result = reclaimer.reconcile(clock.now_ms(), 10).await.unwrap();
    assert_eq!(result.completed, 1);
    assert_eq!(result.deferred, 0);
    assert_eq!(result.retryable_failures, 0);
    assert_eq!(result.conflicts, 0);
    let completed = repository
        .get(&intents[0].intent_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.status, ResourcePurgeStatus::Completed);
    assert_eq!(
        completed.receipt.unwrap().evidence,
        ResourcePurgeEvidence::MemoryStore {
            heads_deleted: 1,
            versions_deleted: 2,
            copy_conflicts_deleted: 1,
        }
    );
    assert!(
        authority
            .memory_snapshot_source()
            .snapshot_heads(store)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        authority
            .memory_versions()
            .list_versions(store)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        authority
            .memory_copy_conflict_queries()
            .list_copy_conflicts(store)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reclaimer
            .reconcile(clock.now_ms(), 10)
            .await
            .unwrap()
            .completed,
        0
    );
}

async fn assert_memory_sentinel(
    application: &awaken_resource_application::ResourcesApplication,
    store: &str,
    before: &(
        Vec<awaken_resource_contract::Memory>,
        Vec<awaken_resource_contract::MemoryVersion>,
        Vec<awaken_resource_contract::MemoryCopyConflict>,
    ),
) {
    let authority = application.authorities();
    let after = (
        authority
            .memory_snapshot_source()
            .snapshot_heads(store)
            .await
            .unwrap(),
        authority
            .memory_versions()
            .list_versions(store)
            .await
            .unwrap(),
        authority
            .memory_copy_conflict_queries()
            .list_copy_conflicts(store)
            .await
            .unwrap(),
    );
    assert_eq!(
        &after, before,
        "AppB exact sentinel survives HTTP and reclamation"
    );
}
