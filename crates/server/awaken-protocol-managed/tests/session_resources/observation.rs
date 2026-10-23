//! Observe canonical Session inputs without triggering preparation.
use super::*;

#[tokio::test]
async fn manifest_observation_is_a_read_of_exact_resources_and_existing_policy() {
    // DT-OBS-HTTP: C1 existing/absent Session; C2 repeated read.
    // R1 existing -> exact desired/applied versions, File identity and
    // structural mutation classes; R2 absent ->404. Neither read changes
    // the root revision, calls Runtime preparation, nor allocates a binding.
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let state = registry_state(runtime).await;
    let app = router(state.clone());
    let created = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a", "resources": [{"type": "file", "file_id": "file_1"}]
        })),
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let queries = state.session_application().queries();
    let before = queries.session(&SessionId::from(id)).await.unwrap();
    let preparations = prepared.lock().unwrap().len();
    for _ in 0..2 {
        let view = call(
            &app,
            "GET",
            &format!("/v1/awaken/sessions/{id}/resources"),
            None,
        )
        .await
        .expect(StatusCode::OK);
        assert_eq!(view["resources"], created["resources"]);
        assert_eq!(view["desired_revision"], view["applied_revision"]);
        assert_eq!(view["phase"], "active");
        assert_eq!(
            view["mutations"],
            json!({"files": true, "replace_manifest": true, "repository_credentials": true})
        );
    }
    let after = queries.session(&SessionId::from(id)).await.unwrap();
    assert_eq!(before.revision, after.revision);
    assert_eq!(before.resources, after.resources);
    assert_eq!(prepared.lock().unwrap().len(), preparations);
    call(&app, "GET", "/v1/awaken/sessions/absent/resources", None)
        .await
        .expect(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn observation_separates_rolled_back_manifest_from_the_mutation_watermark() {
    // DT-ROLLBACK: C1 File generation 1 is active; C2 a live File addition fails
    // and rollback succeeds. R1 retain exact generation-1 resources and paired
    // desired/applied versions; R2 expose watermark 2 for the next If-Match and
    // mark its unapplied change. A read must not enable the rejected File, nor
    // strand the prior usable manifest in a permanent Preparing indication.
    let runtime = AcceptingFake::default();
    let fail_next = runtime.fail_next_apply.clone();
    let state = registry_state(runtime).await;
    let app = router(state.clone());
    let created = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a", "resources": [{"type": "file", "file_id": "file_1"}]
        })),
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    fail_next.store(true, std::sync::atomic::Ordering::SeqCst);
    call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({
            "type": "file", "file_id": "file-rollback", "mount_path": "/rollback.txt"
        })),
    )
    .await
    .expect(StatusCode::INTERNAL_SERVER_ERROR);
    let view = call(
        &app,
        "GET",
        &format!("/v1/awaken/sessions/{id}/resources"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(view["resources"], created["resources"]);
    assert_eq!(view["phase"], "active");
    assert_eq!(view["revision"], 2);
    assert_eq!(view["desired_revision"], 1);
    assert_eq!(view["applied_revision"], 1);
    assert_eq!(view["unapplied_revision"], 2);
    assert_eq!(
        state.list_resources(id).await.unwrap().0,
        2,
        "official list ETag retains the CAS watermark"
    );
}

#[tokio::test]
async fn observation_exposes_selected_input_configuration_without_credential_fields() {
    // DT-DETAIL: File + read-only Memory with instructions -> exact selected
    // identities, access and configuration version. File has no invented
    // content/config version. The allowlisted projection contains no credential
    // material, capability, or current-draft lookup; empty Skills stay empty.
    let state = registry_state(AcceptingFake::default()).await;
    let app = router(state);
    let created = call(&app, "POST", "/v1/sessions", Some(json!({
        "agent": "a", "resources": [
            {"type": "file", "file_id": "file_1"},
            {"type": "memory_store", "memory_store_id": "agent-memory", "access": "read_only", "instructions": "Use accepted release policies."}
        ]
    }))).await.expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let view = call(
        &app,
        "GET",
        &format!("/v1/awaken/sessions/{id}/resources"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    let inputs = view["inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 2);
    let file = inputs
        .iter()
        .find(|input| input["resource_kind"] == "file")
        .unwrap();
    assert_eq!(file["target_id"], "file_1");
    assert!(file.get("configuration_version").is_none());
    let memory = inputs
        .iter()
        .find(|input| input["resource_kind"] == "memory_store")
        .unwrap();
    assert_eq!(memory["target_id"], "agent-memory");
    assert_eq!(memory["configuration_version"], "1");
    assert_eq!(memory["access"], "read_only");
    assert_eq!(memory["instructions"], "Use accepted release policies.");
    for input in inputs {
        for excluded in ["credential", "authorization_token", "capability", "secret"] {
            assert!(
                input.get(excluded).is_none(),
                "not an allowed input observation field: {excluded}"
            );
        }
    }
    assert_eq!(view["skills"], json!([]));
}

#[tokio::test]
async fn inspection_is_opt_in_scoped_and_never_substitutes_a_current_publication() {
    // DT-INSPECT-HTTP: R1 default read -> exact legacy shape; R2 opt-in ->
    // original frozen Environment + explicit missing publication; R3 foreign
    // Workspace ->404. All reads preserve root revision and prepare count.
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let state = registry_state(runtime).await;
    let app = router(state.clone());
    let created = call(&app, "POST", "/v1/sessions", Some(json!({"agent":"a"})))
        .await
        .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let reader = profiled_router(state.clone(), awaken_tenancy::DEFAULT_WORKSPACE_ID);
    let path = format!("/v1/awaken/sessions/{id}/execution-evidence");
    let before = state
        .session_application()
        .queries()
        .session(&SessionId::from(id))
        .await
        .unwrap();
    let preparations = prepared.lock().unwrap().len();
    let legacy = call(&reader, "GET", &path, None)
        .await
        .expect(StatusCode::OK);
    assert!(legacy.get("configuration").is_none());
    let inspected = call(
        &reader,
        "GET",
        &format!("{path}?include_inspection=true"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        inspected["configuration"]["environment"],
        serde_json::to_value(&before.frozen_baseline().unwrap().environment).unwrap()
    );
    assert!(inspected["configuration"]["publication"].is_null());
    call(
        &profiled_router(state.clone(), "foreign"),
        "GET",
        &format!("{path}?include_inspection=true"),
        None,
    )
    .await
    .expect(StatusCode::NOT_FOUND);
    let after = state
        .session_application()
        .queries()
        .session(&SessionId::from(id))
        .await
        .unwrap();
    assert_eq!(before.revision, after.revision);
    assert_eq!(prepared.lock().unwrap().len(), preparations);
}
