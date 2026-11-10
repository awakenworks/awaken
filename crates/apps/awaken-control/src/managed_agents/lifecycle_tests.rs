// @formal surface exclude=test_only
// @formal owner crates/apps/awaken-control/src/managed_agents.rs
// @formal reason: HTTP and storage regressions for one revision-fenced archive command.
use super::test_support::{create_params, plane_with_catalog};
use super::*;
use awaken_protocol_managed::AgentRegistryState;
use awaken_runtime_contract::runs::scope::WorkspaceScope;
use axum::{
    Extension, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use tower::ServiceExt;

#[derive(Default)]
struct CascadeProbe {
    calls: Mutex<Vec<(String, String)>>,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl awaken_deployment_contract::AgentArchiveCascade for CascadeProbe {
    async fn archive_agent_dependents(&self, workspace: &str, agent: &str) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push((workspace.into(), agent.into()));
        if self.fail.load(Ordering::SeqCst) {
            Err("retained cascade unavailable".into())
        } else {
            Ok(())
        }
    }
}

async fn call(app: &Router, workspace: &str, id: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .layer(Extension(WorkspaceScope(workspace.into())))
        .oneshot(
            Request::post(format!("/v1/config/agents/{id}/archive"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn target() -> ModelSelection {
    serde_json::from_value(json!({"mode":"target", "backend_ref":"genai",
        "target":{"model_id":"selected", "provider_id":"provider", "protocol_endpoint_id":"chat", "api_dialect":"open_ai_chat"}})).unwrap()
}

#[tokio::test]
async fn native_archive_preserves_target_and_fences_every_mutation() {
    // DT-ARCHIVE: C1 positive exact generation, C2 trusted owner Workspace,
    // C3 active/archived lifecycle, C4 timestamp/extra-field input, C5 a rich
    // Target. R1 missing/invalid/extra generation payload ->400, zero effects;
    // R2 foreign/reserved scope ->404; R3 stale active generation ->409;
    // R4 exact active rich Target -> one CAS, unchanged model, withdraw and
    // existing cascade, native receipt without a lossy Managed model;
    // R5 original generation or exact Archived revision -> same timestamp and
    // revision, idempotent downstream repair; R6 older generation ->409.
    let temp = tempfile::tempdir().unwrap();
    let (plane, catalog) =
        plane_with_catalog(temp.path().join("config.sqlite").to_str().unwrap()).await;
    let repository = Arc::new(ConfigPlaneManagedAgentRepository::new(
        plane.clone(),
        "workspace-a",
        awaken_time::system_clock(),
    ));
    let created = repository
        .create("workspace-a", create_params("native archive"))
        .await
        .unwrap();
    let scope = ScopeId::from("workspace-a");
    let mut current = plane
        .get_versioned(&scope, &created.id)
        .await
        .unwrap()
        .unwrap();
    current.config.model_binding = target();
    plane
        .put_if_revision(&scope, &current.config, current.revision)
        .await
        .unwrap();
    let before = plane
        .get_versioned(&scope, &created.id)
        .await
        .unwrap()
        .unwrap();
    let cascade = Arc::new(CascadeProbe::default());
    let state = Arc::new(
        AgentRegistryState::from_repository(repository.clone())
            .with_archive_cascade(cascade.clone()),
    );
    let app = crate::config_routes::configuration_lifecycle_router(state);
    for payload in [
        json!({}),
        json!({"generation":0}),
        json!({"generation":"2"}),
        json!({"generation":true}),
        json!({"generation":2.5}),
        json!({"generation":before.revision,"archived_at":"forged"}),
    ] {
        assert_eq!(
            call(&app, "workspace-a", &created.id, payload).await.0,
            StatusCode::BAD_REQUEST,
            "R1"
        );
    }
    for workspace in ["workspace-b", RESERVED_ADMIN_SCOPE] {
        assert_eq!(
            call(
                &app,
                workspace,
                &created.id,
                json!({"generation":before.revision})
            )
            .await
            .0,
            StatusCode::NOT_FOUND,
            "R2"
        );
    }
    assert_eq!(
        call(&app, "workspace-a", &created.id, json!({"generation":1}))
            .await
            .0,
        StatusCode::CONFLICT,
        "R3"
    );
    assert_eq!(
        plane
            .get_versioned(&scope, &created.id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        before.revision
    );
    assert!(cascade.calls.lock().unwrap().is_empty());
    let (status, receipt) = call(
        &app,
        "workspace-a",
        &created.id,
        json!({"generation":before.revision}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R4: {receipt}");
    assert_eq!(receipt["generation"], before.revision + 1);
    assert_eq!(receipt["already_archived"], false);
    assert!(receipt.get("model").is_none());
    let after = plane
        .get_versioned(&scope, &created.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.config.model_binding, before.config.model_binding,
        "R4 exact Target"
    );
    assert_eq!(
        after.config.archived_at.as_deref(),
        receipt["archived_at"].as_str()
    );
    assert!(catalog.current("workspace-a", &created.id).is_none());
    for generation in [before.revision, after.revision] {
        let (status, repeated) = call(
            &app,
            "workspace-a",
            &created.id,
            json!({"generation":generation}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "R5");
        assert_eq!(repeated["generation"], receipt["generation"]);
        assert_eq!(repeated["archived_at"], receipt["archived_at"]);
        assert_eq!(repeated["already_archived"], true);
    }
    assert_eq!(
        call(&app, "workspace-a", &created.id, json!({"generation":1}))
            .await
            .0,
        StatusCode::CONFLICT,
        "R6"
    );
    assert_eq!(cascade.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn native_archive_cascade_failure_keeps_identity_for_same_generation_repair() {
    // DT-REPAIR: canonical CAS/withdraw succeeds, cascade fails ->503 with
    // archived state retained, not a rolled-back/not-started command. Reading
    // the same Agent then continuing the original generation repairs the same
    // cascade without another revision or timestamp. Never mint a replacement.
    let temp = tempfile::tempdir().unwrap();
    let (plane, catalog) =
        plane_with_catalog(temp.path().join("config.sqlite").to_str().unwrap()).await;
    let repository = Arc::new(ConfigPlaneManagedAgentRepository::new(
        plane.clone(),
        "workspace-a",
        awaken_time::system_clock(),
    ));
    let created = repository
        .create("workspace-a", create_params("repair archive"))
        .await
        .unwrap();
    let cascade = Arc::new(CascadeProbe::default());
    cascade.fail.store(true, Ordering::SeqCst);
    let app = crate::config_routes::configuration_lifecycle_router(Arc::new(
        AgentRegistryState::from_repository(repository).with_archive_cascade(cascade.clone()),
    ));
    let payload = json!({"generation":created.version});
    assert_eq!(
        call(&app, "workspace-a", &created.id, payload.clone())
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let before = plane
        .get_versioned(&ScopeId::from("workspace-a"), &created.id)
        .await
        .unwrap()
        .unwrap();
    assert!(before.config.archived_at.is_some());
    assert!(catalog.current("workspace-a", &created.id).is_none());
    cascade.fail.store(false, Ordering::SeqCst);
    let (status, receipt) = call(&app, "workspace-a", &created.id, payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["already_archived"], true);
    assert_eq!(receipt["generation"], before.revision);
    let after = plane
        .get_versioned(&ScopeId::from("workspace-a"), &created.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.config, before.config);
    assert_eq!(after.revision, before.revision);
    assert_eq!(cascade.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn native_archive_repairs_withdrawal_for_a_rich_archived_snapshot() {
    // DT-WITHDRAW: inject only the real canonical archive CAS, representing a
    // process stopping before withdrawal. The Target has no Managed model-id
    // representation. Reusing the reviewed generation through the native HTTP
    // adapter must withdraw the old executable and retain exact archive bytes.
    let temp = tempfile::tempdir().unwrap();
    let (plane, catalog) =
        plane_with_catalog(temp.path().join("config.sqlite").to_str().unwrap()).await;
    let repository = Arc::new(ConfigPlaneManagedAgentRepository::new(
        plane.clone(),
        "workspace-a",
        awaken_time::system_clock(),
    ));
    let created = repository
        .create("workspace-a", create_params("interrupted archive"))
        .await
        .unwrap();
    let scope = ScopeId::from("workspace-a");
    let mut current = plane
        .get_versioned(&scope, &created.id)
        .await
        .unwrap()
        .unwrap();
    current.config.model_binding = target();
    plane
        .put_if_revision(&scope, &current.config, current.revision)
        .await
        .unwrap();
    let before = plane
        .get_versioned(&scope, &created.id)
        .await
        .unwrap()
        .unwrap();
    let mut archived = before.config.clone();
    archived.archived_at = Some("2026-10-23T00:00:00Z".into());
    plane
        .archive_if_revision(&scope, &archived, before.revision)
        .await
        .unwrap();
    assert!(catalog.current("workspace-a", &created.id).is_some());
    let app = crate::config_routes::configuration_lifecycle_router(Arc::new(
        AgentRegistryState::from_repository(repository),
    ));
    let (status, receipt) = call(
        &app,
        "workspace-a",
        &created.id,
        json!({"generation":before.revision}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["already_archived"], true);
    assert!(catalog.current("workspace-a", &created.id).is_none());
    let after = plane
        .get_versioned(&scope, &created.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.config, archived);
    assert_eq!(after.revision, before.revision + 1);
}

#[tokio::test]
async fn execution_evidence_native_read_is_scoped_immutable_and_has_no_effects() {
    // Decision table: owner/foreign/reserved scope × GET/POST. R1 owner GET
    // -> exact published identity with safe execution facts; R2 foreign/reserved
    // GET -> empty, no leakage; R3 POST ->405, no authoring or publication.
    // SDK Agent wire stays unchanged. The query invokes no cascade or runtime.
    let temp = tempfile::tempdir().unwrap();
    let (plane, _) = plane_with_catalog(temp.path().join("config.sqlite").to_str().unwrap()).await;
    let repository = Arc::new(ConfigPlaneManagedAgentRepository::new(
        plane.clone(),
        "workspace-a",
        awaken_time::system_clock(),
    ));
    let agent = repository
        .create("workspace-a", create_params("execution evidence"))
        .await
        .unwrap();
    let cascade = Arc::new(CascadeProbe::default());
    let state = Arc::new(
        AgentRegistryState::from_repository(repository.clone())
            .with_archive_cascade(cascade.clone()),
    );
    let app = crate::config_routes::configuration_lifecycle_router(state);
    for workspace in ["workspace-a", "workspace-b", RESERVED_ADMIN_SCOPE] {
        let response = app
            .clone()
            .layer(Extension(WorkspaceScope(workspace.into())))
            .oneshot(
                Request::get("/v1/awaken/agents/execution-evidence")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let data = body["data"].as_array().unwrap();
        if workspace == "workspace-a" {
            assert_eq!(data.len(), 1);
            assert_eq!(data[0]["agent_id"], agent.id);
            assert_eq!(data[0]["version"], agent.version);
            assert_eq!(data[0]["execution"]["backend_ref"], "genai");
            assert_eq!(data[0]["execution"]["provisioning"], "provider");
            assert_eq!(data[0]["execution"].as_object().unwrap().len(), 3);
        } else {
            assert!(data.is_empty());
        }
    }
    let response = app
        .layer(Extension(WorkspaceScope("workspace-a".into())))
        .oneshot(
            Request::post("/v1/awaken/agents/execution-evidence")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(cascade.calls.lock().unwrap().is_empty());
    assert_eq!(
        repository
            .retrieve("workspace-a", &agent.id, None)
            .await
            .unwrap()
            .version,
        agent.version
    );
}

#[tokio::test]
async fn execution_evidence_can_select_a_frozen_project_publication() {
    // E graph: C1 current/older exact publication; C2 owner/foreign/reserved;
    // C3 complete/missing/invalid selector. E1 exact version remains readable
    // after a new publication; E2 foreign/missing gives no evidence; E3 partial
    // zero or unknown selector refuses before reads. All rules are GET-only and leave
    // the existing publication revision and cascade untouched.
    let temp = tempfile::tempdir().unwrap();
    let (plane, _) = plane_with_catalog(temp.path().join("config.sqlite").to_str().unwrap()).await;
    let repository = Arc::new(ConfigPlaneManagedAgentRepository::new(
        plane.clone(),
        "workspace-a",
        awaken_time::system_clock(),
    ));
    let old = repository
        .create("workspace-a", create_params("pinned project"))
        .await
        .unwrap();
    let current = repository
        .update(
            "workspace-a",
            &old.id,
            super::test_support::update_params(old.version),
        )
        .await
        .unwrap();
    assert!(current.version > old.version);
    let cascade = Arc::new(CascadeProbe::default());
    let app = crate::config_routes::configuration_lifecycle_router(Arc::new(
        AgentRegistryState::from_repository(repository.clone())
            .with_archive_cascade(cascade.clone()),
    ));
    for workspace in ["workspace-a", "workspace-b", RESERVED_ADMIN_SCOPE] {
        for version in [old.version, current.version, current.version + 100] {
            let response = app
                .clone()
                .layer(Extension(WorkspaceScope(workspace.into())))
                .oneshot(
                    Request::get(format!(
                        "/v1/awaken/agents/execution-evidence?agent_id={}&version={version}",
                        old.id
                    ))
                    .body(Body::empty())
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            if workspace == "workspace-a" && version <= current.version {
                assert_eq!(body["data"].as_array().unwrap().len(), 1);
                assert_eq!(body["data"][0]["agent_id"], old.id);
                assert_eq!(body["data"][0]["version"], version);
                assert_eq!(body["data"][0]["execution"]["backend_ref"], "genai");
            } else {
                assert!(body["data"].as_array().unwrap().is_empty());
            }
        }
    }
    for query in [
        format!("agent_id={}", old.id),
        "version=1".into(),
        format!("agent_id={}&version=0", old.id),
        format!("agent_id={}&version=1&unknown_revision=2", old.id),
    ] {
        let response = app
            .clone()
            .layer(Extension(WorkspaceScope("workspace-a".into())))
            .oneshot(
                Request::get(format!("/v1/awaken/agents/execution-evidence?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert!(cascade.calls.lock().unwrap().is_empty());
    assert_eq!(
        repository
            .retrieve("workspace-a", &old.id, None)
            .await
            .unwrap()
            .version,
        current.version
    );
}
