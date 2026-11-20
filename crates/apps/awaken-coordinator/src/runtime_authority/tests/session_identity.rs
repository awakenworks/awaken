//! Managed Session identity round trips through the selected object authority.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt as _;
use std::sync::Arc;
use tower::ServiceExt as _;

fn router(pair: &crate::ObjectAgentPersistence, scope: &str) -> axum::Router {
    let clock = awaken_time::system_clock();
    let probe = Arc::new(awaken_session_application::ExecutionObservationProbe::default());
    let execution = awaken_session_application::SessionExecutionAdmission::new(
        Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone())),
        probe.clone(),
        probe,
    );
    let application = awaken_session_application::SessionApplicationBuilder::new(
        awaken_session_execution_contract::SessionExecutionRoles::colocated(
            Arc::new(awaken_protocol_managed_testkit::CoordinatedRuntimeFake::default()),
            Arc::new(awaken_session_execution_contract::NeutralRuntime),
        ),
        pair.session_persistence(clock.clone()),
        awaken_protocol_managed_testkit::environment_fixture(clock).execution,
        execution,
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .unwrap();
    let state = Arc::new(awaken_protocol_managed::ManagedState::from_application(
        Arc::new(application),
        Default::default(),
    ));
    awaken_protocol_managed::router(state).layer(axum::Extension(
        awaken_runtime_contract::runs::scope::WorkspaceScope(scope.into()),
    ))
}

async fn json_response(response: axum::response::Response) -> (StatusCode, serde_json::Value) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_object_sessions_use_uuid_ids_and_preserve_legacy_ids_after_restart() {
    // I1 actual Managed create -> safe UUIDv7 ID; I2 cold GET -> same ID;
    // I3 an existing fingerprint ID remains readable; I4 another Workspace
    // cannot observe either identity. No LLM or Sandbox is needed to create.
    let scope = awaken_tenancy::DEFAULT_WORKSPACE_ID;
    let engine =
        awaken_store_s3::test_support::storage("managed-identities", awaken_time::system_clock());
    let pair = crate::object_agent_persistence(engine.clone(), scope).unwrap();
    let app = router(&pair, scope);
    let response = Box::pin(app.clone().oneshot(Request::builder()
        .method("POST").uri("/v1/sessions").header("content-type", "application/json")
        .body(Body::from(serde_json::json!({
            "agent": "coder", "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        }).to_string())).unwrap())).await.unwrap();
    let (status, created) = json_response(response).await;
    assert_eq!(status, StatusCode::OK, "I1: {created}");
    let id = created["id"].as_str().unwrap().to_owned();
    let suffix = id.strip_prefix("sesn_").unwrap();
    assert_eq!(suffix.len(), 32);
    assert_eq!(uuid::Uuid::parse_str(suffix).unwrap().get_version_num(), 7);
    assert!(engine.find_session(&id).await.unwrap().is_some());

    let legacy_id = "sesn_fnv1a64:0123456789abcdef";
    pair.sessions()
        .create(
            scope,
            awaken_protocol_managed_testkit::sample_persisted(legacy_id),
            awaken_session_contract::IdempotencyRecord {
                key: "legacy-create".into(),
                payload_hash: "legacy-create".into(),
            },
            vec![],
        )
        .await
        .unwrap();
    drop(app);
    drop(pair);

    let cold = crate::object_agent_persistence(engine, scope).unwrap();
    let app = router(&cold, scope);
    for expected in [id.as_str(), legacy_id] {
        let response = Box::pin(
            app.clone().oneshot(
                Request::builder()
                    .uri(format!("/v1/sessions/{expected}"))
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
        let (status, restored) = json_response(response).await;
        assert_eq!(status, StatusCode::OK, "I2/I3: {restored}");
        assert_eq!(restored["id"], expected);
    }
    let foreign = router(&cold, "other-workspace");
    for expected in [id.as_str(), legacy_id] {
        let response = Box::pin(
            foreign.clone().oneshot(
                Request::builder()
                    .uri(format!("/v1/sessions/{expected}"))
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "I4");
    }
}
