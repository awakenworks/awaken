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
    let durable_bindings = |root: &awaken_session_contract::PersistedSession| {
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

#[tokio::test]
async fn create_and_manifest_preserve_distinct_participant_prefixes_on_resolution_failure() {
    // Cause/effect graph: C1 POST creation / PUT complete manifest; C2 the
    // middle MemoryStore passes the shared normalizer, then its real Registry
    // state becomes Suspended during the first Repository material ingress;
    // C3 a second valid Repository follows it. The original Registry resolver,
    // not a replacement resolver or a missing fixture, produces the first error.
    //
    // | Rule | C1 | C2 | C3 | configured/retired Repo prefix | root/Runtime |
    // |---|---|---|---|---|---|
    // | PFX1 | POST | yes | yes | before + after | unchanged |
    // | PFX2 | PUT | yes | yes | before only; after absent | unchanged |
    // Creation lowers all participants before its one final resolver; manifest
    // lowering resolves each row before advancing. Both must return the same
    // original NotActive error and compensate only their actual owned prefix.
    // Existing successful routes cover C2=false; N4 covers preflight refusal.
    const BEFORE: &str = "https://github.com/awaken/prefix-before.git";
    const AFTER: &str = "https://github.com/awaken/prefix-after.git";
    for whole_manifest in [false, true] {
        let runtime = AcceptingFake::default();
        let sessions = support::session_repository("participant prefix Session store").await;
        let catalog = resource_registry().await;
        let ingress = Arc::new(CountingCredentialMaterialIngress::default());
        let clock = awaken_time::system_clock();
        let state = awaken_protocol_managed_testkit::managed_state_from_parts(
            Arc::new(runtime.clone()),
            None,
            sessions.clone(),
            awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
            clock,
            Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .with_resource_registry(catalog.clone())
        .with_credential_material_ingress(ingress.clone());
        let app = router(Arc::new(state));
        let id = if whole_manifest {
            let created = call(&app, "POST", "/v1/sessions", Some(json!({"agent": "a"})))
                .await
                .expect(StatusCode::OK);
            Some(created["id"].as_str().unwrap().to_string())
        } else {
            None
        };
        let roots_before = sessions
            .scan(awaken_session_contract::SessionScanQuery::Workspace {
                workspace_id: "default",
            })
            .await
            .map(awaken_session_contract::SessionScan::into_sessions)
            .unwrap();
        let prepared_before = runtime.prepared.lock().unwrap().len();
        let staged_before = runtime.staged.lock().unwrap().len();
        let applied_before = runtime.applied.lock().unwrap().len();
        *ingress.memory_change_after_first_write.lock().unwrap() = Some((
            catalog.clone(),
            awaken_resource_contract::ChangeMemoryStoreState {
                workspace_id: "default".into(),
                id: "mem_1".into(),
                state: ResourceState::Suspended,
            },
        ));
        let resources = json!([
            {"type": "github_repository", "url": BEFORE,
                "mount_path": "/workspace/prefix-before",
                "authorization_token": "prefix-before-fixture" // awaken-allow(secret): deliberate test fixture
            },
            {"type": "memory_store", "memory_store_id": "mem_1"},
            {"type": "github_repository", "url": AFTER,
                "mount_path": "/workspace/prefix-after",
                "authorization_token": "prefix-after-fixture" // awaken-allow(secret): deliberate test fixture
            }
        ]);
        let (method, uri, body) = if let Some(id) = &id {
            (
                "PUT",
                format!("/v1/awaken/sessions/{id}/resources"),
                json!({"resources": resources}),
            )
        } else {
            (
                "POST",
                "/v1/sessions".to_string(),
                json!({"agent": "a", "resources": resources}),
            )
        };
        let rejected = call(&app, method, &uri, Some(body))
            .await
            .expect(StatusCode::BAD_REQUEST);
        assert!(
            rejected
                .to_string()
                .contains("resource `mem_1` is not active (Suspended)"),
            "PFX1/PFX2 original resolver error: {rejected}"
        );
        assert_eq!(
            catalog
                .find_memory_store("default", "mem_1")
                .await
                .unwrap()
                .unwrap()
                .state,
            ResourceState::Suspended,
            "PFX1/PFX2 actual Registry change, not a resolver stub"
        );
        let expected_prefix = if whole_manifest { 1 } else { 2 };
        let sources = ingress.sources.lock().unwrap().clone();
        assert_eq!(sources.len(), expected_prefix, "PFX1/PFX2 ingress order");
        assert_eq!(
            ingress.writes.load(std::sync::atomic::Ordering::SeqCst),
            expected_prefix
        );
        assert_eq!(
            ingress
                .retirements
                .load(std::sync::atomic::Ordering::SeqCst),
            expected_prefix
        );
        for (source, expected_remote) in sources.iter().zip([BEFORE, AFTER]) {
            let repository_id = source.strip_suffix(":credential").unwrap();
            let definition = catalog
                .find_repository("default", repository_id)
                .await
                .unwrap()
                .unwrap();
            let config = catalog
                .find_repository_config("default", repository_id, ConfigVersion::INITIAL)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                definition.state,
                ResourceState::Deleted,
                "PFX1/PFX2 owned compensation"
            );
            assert_eq!(
                config.remote_url, expected_remote,
                "PFX1/PFX2 exact ordered participant"
            );
        }
        if let Some(id) = id {
            // Use the original public fingerprint function and original tuple
            // to address the later participant that PUT must never register.
            let later = format!(
                "managed:{id}:repository:{}",
                awaken_agent_contract::stable_fingerprint(&(
                    "workspace/prefix-after",
                    AFTER,
                    None::<String>,
                    None::<String>
                ))
            );
            assert!(
                catalog
                    .find_repository("default", &later)
                    .await
                    .unwrap()
                    .is_none(),
                "PFX2 no later Registry participant, including a tombstone"
            );
        }
        assert_eq!(
            sessions
                .scan(awaken_session_contract::SessionScanQuery::Workspace {
                    workspace_id: "default",
                })
                .await
                .map(awaken_session_contract::SessionScan::into_sessions)
                .unwrap(),
            roots_before,
            "PFX1 no new root; PFX2 exact original root, including revision and resources"
        );
        assert_eq!(
            runtime.prepared.lock().unwrap().len(),
            prepared_before,
            "PFX1/PFX2 no preparation"
        );
        assert_eq!(
            runtime.staged.lock().unwrap().len(),
            staged_before,
            "PFX1/PFX2 no staging"
        );
        assert_eq!(
            runtime.applied.lock().unwrap().len(),
            applied_before,
            "PFX1/PFX2 no apply"
        );
    }
}
