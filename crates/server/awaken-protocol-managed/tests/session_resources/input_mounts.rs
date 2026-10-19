//! The original POST and PUT routes consume one application path normalizer.
use super::*;

#[tokio::test]
async fn manifest_memory_mounts_reuse_creation_paths_and_binding_identities() {
    // N3/N6 route table: two governed definitions share one display slug;
    // POST derives base/id paths; PUT of the same complete authored set uses
    // the identical paths and retains the existing bindings. This is actual
    // Managed parsing -> original application role -> Registry -> root CAS,
    // not a hand-built normalized request or another resolver implementation.
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let state = registry_state(runtime).await;
    let app = router(state.clone());
    // MemoryStore's original wire has memory_store_id and no addressable id.
    // N3 checks that actual wire; N6 reads binding_id from the same durable
    // Queries owner before/after PUT, never inventing a wire binding identity.
    let wire_memories = |response: &serde_json::Value| {
        response["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|input| {
                (
                    input["memory_store_id"].as_str().unwrap().to_string(),
                    input["mount_path"].as_str().unwrap().to_string(),
                )
            })
            .collect::<Vec<_>>()
    };
    let durable_bindings = |root: &awaken_session_state_contract::PersistedSession| {
        root.resources
            .desired()
            .inputs()
            .iter()
            .map(|input| (input.binding_id.to_string(), input.mount_path.clone()))
            .collect::<Vec<_>>()
    };
    let resources = json!([
        {"type": "memory_store", "memory_store_id": "mem_same_1"},
        {"type": "memory_store", "memory_store_id": "mem_same_2"}
    ]);
    let created = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a", "resources": resources.clone()
        })),
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let original_wire = wire_memories(&created);
    let original = durable_bindings(
        &state
            .session_application()
            .queries()
            .session(&SessionId::from(id))
            .await
            .unwrap(),
    );
    assert_eq!(
        original
            .iter()
            .map(|(_, path)| path.as_str())
            .collect::<Vec<_>>(),
        vec![
            "/mnt/memory/project-memory",
            "/mnt/memory/project-memory-mem-same-2"
        ],
        "N3 original POST paths"
    );
    let replaced = call(
        &app,
        "PUT",
        &format!("/v1/awaken/sessions/{id}/resources"),
        Some(json!({"resources": resources})),
    )
    .await
    .expect(StatusCode::OK);
    let after_wire = wire_memories(&replaced);
    let after = durable_bindings(
        &state
            .session_application()
            .queries()
            .session(&SessionId::from(id))
            .await
            .unwrap(),
    );
    assert_eq!(
        after, original,
        "N6 existing manifest binding identity is not create-index identity"
    );
    assert_eq!(
        after_wire, original_wire,
        "N6 original wire MemoryStore identity/path"
    );
    assert_eq!(
        original_wire
            .iter()
            .map(|(_, path)| path.as_str())
            .collect::<Vec<_>>(),
        original
            .iter()
            .map(|(_, path)| path.as_str())
            .collect::<Vec<_>>(),
        "N3 wire paths agree with original durable bindings"
    );
    assert_eq!(
        prepared.lock().unwrap()[0]
            .resources
            .inputs()
            .iter()
            .map(|input| input.mount_path.clone())
            .collect::<Vec<_>>(),
        vec![
            "/mnt/memory/project-memory",
            "/mnt/memory/project-memory-mem-same-2"
        ],
        "N3 frozen Runtime paths"
    );
}

#[tokio::test]
async fn manifest_repository_preflight_precedes_missing_memory_lookup_and_participants() {
    // N4: an earlier derived but missing MemoryStore precedes a later invalid
    // Repository path with write-only material. The WHOLE Repository preflight
    // must retain precedence over definition lookup (the observed error names
    // /repo, not the missing definition), and reject before Registry/Vault,
    // Runtime apply, or any root change. Existing POST test owns the paired
    // create rule; this test exercises the real complete-manifest PUT route.
    let runtime = AcceptingFake::default();
    let applied = runtime.applied.clone();
    let sessions = support::session_repository("manifest preflight Session store").await;
    let credentials = Arc::new(CountingCredentialMaterialIngress::default());
    let state = {
        let session_fixture_runtime = runtime;
        let session_fixture_clock = awaken_time::system_clock();
        awaken_protocol_managed_testkit::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            sessions.clone(),
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    }
    .with_resource_registry(resource_registry().await)
    .with_credential_material_ingress(credentials.clone());
    let app = router(Arc::new(state));
    let created = call(&app, "POST", "/v1/sessions", Some(json!({"agent": "a"})))
        .await
        .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let before = sessions.get(&SessionId::from(id)).await.unwrap();
    let applied_before = applied.lock().unwrap().len();
    let rejected = call(
        &app,
        "PUT",
        &format!("/v1/awaken/sessions/{id}/resources"),
        Some(json!({
            "resources": [
                {"type": "memory_store", "memory_store_id": "missing-before-repository"},
                {"type": "github_repository", "url": "https://github.com/awaken/invalid-path.git",
                 "mount_path": "/repo", "authorization_token": "must-never-enter-vault"} // awaken-allow(secret):
            ]
        })),
    )
    .await
    .expect(StatusCode::BAD_REQUEST);
    assert!(
        rejected.to_string().contains("/repo"),
        "N4 Repository preflight retains precedence: {rejected}"
    );
    assert_eq!(
        credentials.writes.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "N4 no Vault entry"
    );
    assert_eq!(
        credentials
            .retirements
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "N4 no compensation needed"
    );
    assert_eq!(
        applied.lock().unwrap().len(),
        applied_before,
        "N4 no Runtime effect"
    );
    assert_eq!(
        sessions.get(&SessionId::from(id)).await.unwrap(),
        before,
        "N4 exact root unchanged"
    );
}
