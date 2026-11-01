//! Both real HTTP create entries enter the sole creation Resource finalizer.
use super::*;

const REMOTE: &str = "https://github.com/awaken/finalized.git";

fn profiled_request(
    id: &str,
    agent: &str,
    credential: Option<awaken_agent_contract::credential::CredentialRef>,
) -> Value {
    json!({
        "session_id": id, "mode": "work_unit", "agent_id": agent,
        "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        "repositories": [{ "remote_url": REMOTE, "mount_path": "/workspace/finalized",
            "credential": credential, "binding_id": "original-profiled-binding" }]
    })
}

#[tokio::test]
async fn both_create_routes_finalize_resources_before_root_or_runtime_effects() {
    // F1/F4 real-entry table: requested vs Profiled, each with a configured
    // Repository. F1 Agent default names an absent platform Repository and so
    // resolution fails before missing credential source; F4 no such default
    // reaches actual pinning and fails with the original configured-Vault
    // message. Both retain exact HTTP BadRequest, canonical owned Repository
    // compensation, zero root and zero Runtime. Ordinary inline ingress is
    // retired once; Profiled's externally authored reference is never retired.
    for profiled in [false, true] {
        for resolution_failure in [true, false] {
            let runtime = AcceptingFake::default();
            let prepared = runtime.prepared.clone();
            let sessions = support::session_repository("create finalizer route store").await;
            let catalog = resource_registry().await;
            let ingress = Arc::new(CountingCredentialMaterialIngress::default());
            let mut state = {
                let session_fixture_runtime = runtime;
                let session_fixture_clock = awaken_time::system_clock();
                awaken_protocol_managed_testkit::managed_state_from_parts(
                    std::sync::Arc::new(session_fixture_runtime),
                    None,
                    sessions.clone(),
                    awaken_protocol_managed_testkit::environment_fixture(
                        session_fixture_clock.clone(),
                    )
                    .execution,
                    session_fixture_clock,
                    std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                )
            }
            .with_resource_registry(catalog.clone())
            .with_credential_material_ingress(ingress.clone());
            if resolution_failure {
                state = state.with_config_source(Arc::new(AgentWithPlatformRepository));
            }
            let state = Arc::new(state);
            let agent = if resolution_failure {
                "repo-agent"
            } else {
                "a"
            };
            let (app, uri, body) = if profiled {
                (
                    profiled_router(state, "default"),
                    "/v1/awaken/sessions",
                    profiled_request(
                        "finalizer-profiled-failure",
                        agent,
                        Some(awaken_agent_contract::credential::CredentialRef {
                            id: "external-original-source".into(),
                            revision: 1,
                        }),
                    ),
                )
            } else {
                (
                    router(state),
                    "/v1/sessions",
                    json!({
                        "agent": agent, "resources": [{"type": "github_repository", "url": REMOTE,
                            "mount_path": "/workspace/finalized", "authorization_token": "finalizer-fixture-token"}] // awaken-allow(secret):
                    }),
                )
            };
            let error = call(&app, "POST", uri, Some(body))
                .await
                .expect(StatusCode::BAD_REQUEST);
            let message = error.to_string();
            assert!(
                if resolution_failure {
                    message.contains("platform-repository")
                } else {
                    message.contains("Repository credential requires a configured credential vault")
                },
                "F1/F4 profiled={profiled} resolution={resolution_failure}: {message}"
            );
            let repository_id = if profiled {
                "profiled:finalizer-profiled-failure:repository:0".to_string()
            } else {
                ingress.sources.lock().unwrap()[0]
                    .strip_suffix(":credential")
                    .unwrap()
                    .to_string()
            };
            let owner = awaken_session_application::SessionRepositoryOwner::from_repository_id(
                &repository_id,
            )
            .unwrap();
            assert_eq!(
                catalog
                    .find_repository("default", &repository_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                ResourceState::Deleted,
                "original Applied compensation"
            );
            assert!(
                matches!(
                    sessions.get(&SessionId::from(owner.session_id())).await,
                    Err(awaken_session_contract::SessionRepositoryError::NotFound)
                ),
                "no root"
            );
            assert!(
                prepared.lock().unwrap().is_empty(),
                "no Runtime preparation"
            );
            assert_eq!(
                ingress.writes.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(!profiled)
            );
            assert_eq!(
                ingress
                    .retirements
                    .load(std::sync::atomic::Ordering::SeqCst),
                usize::from(!profiled)
            );
        }
    }
}

#[tokio::test]
async fn both_create_routes_pin_exact_credentials_and_profiled_drift_stays_after_finalization() {
    // F5/F7 use real Vault/Registry/SQLite and both original HTTP entries.
    // Ordinary inline material -> exact r1 pin; Profiled authors that same
    // reference -> same holder/target/revision, own binding identity. Rotate
    // through original Vault CAS to r2, then author stale r1 in a NEW Profiled
    // request: actual pin succeeds at r2, original later drift check rejects
    // before root/runtime and retires only its own Registry participant. The
    // shared credential stays Active r2; neither existing root is rewritten.
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let vaults = Arc::new(support::vault(secrets, credentials.clone()));
    let catalog = resource_registry().await;
    let sessions = support::session_repository("exact creation finalizer store").await;
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let state = Arc::new(
        {
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
        .with_resource_registry(catalog.clone())
        .with_vaults(vaults.clone()),
    );
    let ordinary = router(state.clone());
    let profiled = profiled_router(state, "default");
    let created = call(
        &ordinary,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a", "resources": [{"type": "github_repository", "url": REMOTE,
                "mount_path": "/workspace/finalized", "authorization_token": "finalizer-real-vault-token"}] // awaken-allow(secret):
        })),
    )
    .await
    .expect(StatusCode::OK);
    let original_id = created["id"].as_str().unwrap();
    let original = sessions.get(&SessionId::from(original_id)).await.unwrap();
    let awaken_session_configuration_contract::resource::ResolvedInputSource::Repository {
        credential: Some(pin),
        ..
    } = &original.resources.active.inputs()[0].source
    else {
        panic!("F5 real exact pin")
    };
    assert_eq!(pin.access.credential.revision, 1);
    let reference = pin.access.credential.clone();
    call(
        &profiled,
        "POST",
        "/v1/awaken/sessions",
        Some(profiled_request(
            "finalizer-profiled-success",
            "a",
            Some(reference.clone()),
        )),
    )
    .await
    .expect(StatusCode::ACCEPTED);
    let profiled_root = sessions
        .get(&SessionId::from("finalizer-profiled-success"))
        .await
        .unwrap();
    let desired = profiled_root.resources.desired();
    let [input] = desired.inputs() else {
        panic!("F5 original Profiled input")
    };
    assert_eq!(input.binding_id.as_str(), "original-profiled-binding");
    assert_eq!(input.mount_path, "/workspace/finalized");
    let awaken_session_configuration_contract::resource::ResolvedInputSource::Repository {
        credential: Some(profiled_pin),
        ..
    } = &input.source
    else {
        panic!("F5 exact Profiled pin")
    };
    assert_eq!(
        profiled_pin, pin,
        "F5 same original reference/target/policy/holder"
    );
    let source_id = awaken_credential_contract::CredentialSourceId(reference.id.clone());
    let next_revision = awaken_credential_contract::CredentialMaterialIngress::rotate_material(vaults.application().credential_access().as_ref(), awaken_credential_contract::CredentialMaterialRotationCommand {
        source_id: source_id.clone(), expected_revision: reference.revision, workspace_id: "default".into(),
        target: awaken_session_configuration_contract::resource::repository_transport_credential_target(REMOTE).unwrap(),
        usage: awaken_session_configuration_contract::resource::repository_transport_credential_usage(),
        material: awaken_credential_contract::CredentialMaterialInput::structured("github",
            awaken_credential_contract::http_basic_material(
                awaken_agent_contract::RedactedString::new("x-access-token"),
                awaken_agent_contract::RedactedString::new("rotated-finalizer-fixture-token"))),
    }).await.unwrap();
    assert_eq!(next_revision, 2);
    let preparations = prepared.lock().unwrap().len();
    let error = call(
        &profiled,
        "POST",
        "/v1/awaken/sessions",
        Some(profiled_request(
            "finalizer-profiled-drift",
            "a",
            Some(reference),
        )),
    )
    .await
    .expect(StatusCode::BAD_REQUEST);
    assert!(
        error
            .to_string()
            .contains("credential revision changed before Session admission"),
        "F7 {error}"
    );
    assert_eq!(
        prepared.lock().unwrap().len(),
        preparations,
        "F7 no Runtime"
    );
    assert!(
        matches!(
            sessions
                .get(&SessionId::from("finalizer-profiled-drift"))
                .await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "F7 no root"
    );
    assert_eq!(
        catalog
            .find_repository("default", "profiled:finalizer-profiled-drift:repository:0")
            .await
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Deleted,
        "F7 only failed creation participant retired"
    );
    let source = credentials.get(&source_id).await.unwrap();
    assert_eq!(source.version, 2);
    assert_eq!(
        source.status,
        awaken_credential_vault::CredentialStatus::Active,
        "F7 shared credential retained"
    );
    assert_eq!(
        sessions.get(&SessionId::from(original_id)).await.unwrap(),
        original,
        "F7 original root unchanged"
    );
    assert_eq!(
        sessions
            .get(&SessionId::from("finalizer-profiled-success"))
            .await
            .unwrap(),
        profiled_root,
        "F7 existing Profiled root unchanged"
    );
}

/// The ordering oracle for one Session id: the effects a root committed before
/// Resource finalization leaves behind. The store exposes no root removal, so
/// a root written first and compensated later is still visible here.
async fn creation_effects(
    sessions: &SqliteManagedSessionRepository,
    session_id: &str,
) -> Vec<&'static str> {
    use awaken_session_contract::LifecycleOutbox as _;
    let mut effects = Vec::new();
    if sessions.get(&SessionId::from(session_id)).await.is_ok() {
        effects.push("root");
    }
    if sessions
        .pending_lifecycle()
        .await
        .expect("lifecycle outbox is readable")
        .iter()
        .any(|fact| fact.object_id == session_id)
    {
        effects.push("lifecycle fact");
    }
    effects
}

/// | Rule | route | finalization | Effect |
/// |---|---|---|---|
/// | O1 | requested | credential pinning fails | no root, no atomic lifecycle fact, no Runtime |
/// | O2 | Profiled | credential pinning fails | same as O1 |
/// | NC | requested | a root committed before the check | the same oracle reports root and fact |
#[tokio::test]
async fn failed_finalization_commits_no_root_or_lifecycle_fact_on_either_route() {
    for profiled in [false, true] {
        let runtime = AcceptingFake::default();
        let prepared = runtime.prepared.clone();
        let sessions = support::session_repository("creation ordering store").await;
        let catalog = resource_registry().await;
        let ingress = Arc::new(CountingCredentialMaterialIngress::default());
        let state = Arc::new(
            {
                let session_fixture_clock = awaken_time::system_clock();
                awaken_protocol_managed_testkit::managed_state_from_parts(
                    std::sync::Arc::new(runtime),
                    None,
                    sessions.clone(),
                    awaken_protocol_managed_testkit::environment_fixture(
                        session_fixture_clock.clone(),
                    )
                    .execution,
                    session_fixture_clock,
                    std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                )
            }
            .with_resource_registry(catalog)
            .with_credential_material_ingress(ingress.clone()),
        );
        let (app, uri, body) = if profiled {
            (
                profiled_router(state.clone(), "default"),
                "/v1/awaken/sessions",
                profiled_request(
                    "ordering-profiled-failure",
                    "a",
                    Some(awaken_agent_contract::credential::CredentialRef {
                        id: "external-ordering-source".into(),
                        revision: 1,
                    }),
                ),
            )
        } else {
            (
                router(state.clone()),
                "/v1/sessions",
                json!({
                    "agent": "a", "resources": [{"type": "github_repository", "url": REMOTE,
                        "mount_path": "/workspace/finalized", "authorization_token": "ordering-fixture-token"}] // awaken-allow(secret):
                }),
            )
        };
        call(&app, "POST", uri, Some(body))
            .await
            .expect(StatusCode::BAD_REQUEST);
        let session_id = if profiled {
            "ordering-profiled-failure".to_string()
        } else {
            let repository_id = ingress.sources.lock().unwrap()[0]
                .strip_suffix(":credential")
                .unwrap()
                .to_string();
            awaken_session_application::SessionRepositoryOwner::from_repository_id(&repository_id)
                .unwrap()
                .session_id()
                .to_string()
        };
        assert_eq!(
            creation_effects(&sessions, &session_id).await,
            Vec::<&str>::new(),
            "O1/O2 profiled={profiled}"
        );
        assert!(prepared.lock().unwrap().is_empty(), "O1/O2 no Runtime");

        // NC: a root that is committed is visible to the same oracle, so a
        // root-before-finalization ordering cannot pass O1/O2.
        let created = call(
            &router(state),
            "POST",
            "/v1/sessions",
            Some(json!({ "agent": "a" })),
        )
        .await
        .expect(StatusCode::OK);
        let committed = created["id"].as_str().expect("created id");
        assert_eq!(
            creation_effects(&sessions, committed).await,
            ["root", "lifecycle fact"],
            "NC profiled={profiled} the oracle observes a committed root"
        );
    }
}
