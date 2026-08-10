//! File Resource Coordinator boundary tests over real HTTP.

use awaken_run_ingress_testkit::worker_http as support;

use std::sync::Arc;

use awaken_file_store::FileStore as _;
use awaken_resource_application::ApplicationFileContentSource;
use awaken_resource_contract::FileCatalog as _;
use awaken_resource_contract::FileContentSource as _;
use awaken_resource_worker_http::HttpFileContentSource;
use awaken_resource_worker_http::{WorkerFileContentService, worker_file_content_router};
use awaken_run_ingress::{
    DispatchQueue as _, MemoryDispatchStore, RunClaim, RunDispatch, WorkerIdentity,
};
use awaken_session_contract::ManagedSessionRepository as _;
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

fn resources(file_id: &str) -> awaken_session_contract::ResolvedSessionResources {
    awaken_session_contract::ResolvedSessionResources {
        inputs: vec![awaken_session_contract::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::new("file-binding"),
            source: awaken_session_contract::ResolvedInputSource::File {
                file_id: awaken_resource_contract::FileId::from(file_id),
            },
            mount_path: "/workspace/input.txt".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }],
        skills: Some(Vec::new()),
    }
}

async fn claimed_dispatch(
    dispatch: &Arc<MemoryDispatchStore>,
    file_id: &str,
    owner: &str,
) -> RunClaim {
    let request = RunDispatch::new(support::activation(file_id))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-file"),
        ))
        .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
            "workspace-file",
            serde_json::to_string(&resources(file_id)).unwrap(),
        ));
    dispatch.enqueue(request).await.unwrap();
    let claimed = dispatch
        .claim(owner, 60_000, support::unix_now_ms(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    RunClaim::from(&claimed.lease)
}

async fn claimed_application_dispatch(
    dispatch: &Arc<MemoryDispatchStore>,
    owner: &str,
    session_id: &str,
) -> RunClaim {
    let request = RunDispatch::new(support::activation(session_id))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-file"),
        ))
        .for_session(awaken_agent_contract::agent::thread::Id(session_id.into()));
    dispatch.enqueue(request).await.unwrap();
    let claimed = dispatch
        .claim(owner, 60_000, support::unix_now_ms(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    RunClaim::from(&claimed.lease)
}

fn frozen_application_session(
    session_id: &str,
    file_id: &str,
    identity: &WorkerIdentity,
) -> awaken_session_contract::PersistedSession {
    let environment = awaken_session_contract::EnvironmentSnapshot {
        environment_id: "env-worker".into(),
        revision: awaken_environment_contract::EnvironmentRevision(1),
        self_hosted: true,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-1".into()),
        sandbox: serde_json::json!({}),
        sandbox_provisioning: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    let mut resource_state = awaken_session_contract::SessionResourceState::default();
    resource_state
        .prepare(session_id, resources(file_id))
        .expect("prepare application Session resources");
    awaken_session_contract::PersistedSession {
        session_id: session_id.into(),
        revision: Default::default(),
        baseline: awaken_session_contract::SessionBaselineState::Frozen(
            awaken_session_contract::SessionBaseline::compile(
                awaken_session_contract::SessionBaselineInputs {
                    environment,
                    runtime_placement: awaken_session_contract::SessionRuntimePlacement::Worker,
                    mcp_authoring: Default::default(),
                    agent_id: "agent".into(),
                    model: "model".into(),
                    runtime: None,
                    application: Some(awaken_session_contract::ApplicationContributionReceipt {
                        plan_fingerprint: "plan".into(),
                        input_fingerprint: "input".into(),
                    }),
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                },
            ),
        ),
        title: None,
        metadata: Default::default(),
        tools: Default::default(),
        activity_epoch: 0,
        environment: Default::default(),
        mcp: Default::default(),
        resources: resource_state,
        realization: Some(awaken_session_contract::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 1,
            expires_at_unix_ms: support::unix_now_ms().saturating_add(60_000),
        }),
        realization_progress: Default::default(),
        execution: awaken_session_contract::SessionExecutionState::Running,
        disposition: Default::default(),
        terminal_cleanup: Default::default(),
    }
}

/// Cause/effect decision table:
/// | Rule | Worker auth | live exact claim | Workspace/File frozen | content state | Effect |
/// |---|---|---|---|---|---|
/// | F1 | valid | yes | yes | exact | return immutable bytes and digest |
/// | F2 | valid | yes | another File | any | deny before File source |
/// | F3 | missing | any | any | any | HTTP 401 before claim/store |
/// | F4 | valid | stale | yes | any | reject without content |
/// | F5 | valid | yes | yes | substituted response | Worker rejects bytes |
/// | F6 | stale incarnation | yes | yes | exact | deny before File source |
/// | F7 | transport-only | supplied | supplied | digest header missing | Worker rejects response |
/// | F8 | valid | yes | yes | logical File absent | return not-found without bytes |
/// | F9 | valid | yes | yes | record references absent blob | surface unavailable; no partial bytes |
#[tokio::test]
async fn exact_file_content_is_scope_and_claim_fenced_and_digest_verified() {
    let store = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let digest = store.put(b"exact-file").await.unwrap();
    store
        .create_file(awaken_resource_contract::FileRecord {
            id: "file-public".into(),
            workspace_id: "workspace-file".into(),
            blob_id: digest.clone(),
            filename: "input.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 10,
            created_at: "2026-07-30T00:00:00Z".into(),
            downloadable: false,
            scope_id: None,
            logical_path: None,
            harvest_key: None,
            deleted: false,
        })
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-file").await;
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let claim = claimed_dispatch(&dispatch, "file-public", &identity.lease_owner()).await;
    let lifecycle = Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        store.clone(),
        store.clone(),
        lifecycle,
    ));
    let source = Arc::new(ApplicationFileContentSource::new(application));
    let service = Arc::new(
        WorkerFileContentService::new(
            source,
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory),
    );
    let address = support::serve(worker_file_content_router(service)).await;
    let source = HttpFileContentSource::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
    );

    let exact = source
        .read("workspace-file", "file-public", Some(&claim))
        .await
        .expect("F1 exact File read")
        .expect("F1 existing File");
    assert_eq!(exact, (digest.clone(), b"exact-file".to_vec()), "F1");

    let denied = source
        .read("workspace-file", "file-other", Some(&claim))
        .await
        .expect_err("F2 non-frozen File must be denied");
    assert!(denied.to_string().contains("403"), "F2: {denied}");

    let unauthenticated = reqwest::Client::new()
        .post(format!(
            "http://{address}/v1/worker/resources/files/content"
        ))
        .json(&serde_json::json!({
            "claim": claim,
            "identity": identity,
            "workspace_id": "workspace-file",
            "file_id": "file-public"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauthenticated.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "F3"
    );

    let stale_source =
        HttpFileContentSource::new(
            WorkerUpstream::new(format!("http://{address}"))
                .with_worker_identity(WorkerIdentity::new("worker-file", "worker-file-stale", 2)),
        );
    let stale_identity = stale_source
        .read("workspace-file", "file-public", Some(&claim))
        .await
        .expect_err("F6 stale incarnation must be denied");
    assert!(stale_identity.to_string().contains("403"), "F6");

    dispatch
        .settle(
            &claim.run_id,
            claim.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    let stale = source
        .read("workspace-file", "file-public", Some(&claim))
        .await
        .expect_err("F4 stale claim must be rejected");
    assert!(stale.to_string().contains("409"), "F4: {stale}");

    let substituted = axum::Router::new().route(
        "/v1/worker/resources/files/content",
        axum::routing::post(move || async move {
            (
                [("x-awaken-file-content-digest", digest)],
                b"different-file".to_vec(),
            )
        }),
    );
    let substituted_address = support::serve(substituted).await;
    let substituted_source = HttpFileContentSource::new(
        WorkerUpstream::new(format!("http://{substituted_address}")).with_worker_id("worker-file"),
    );
    assert!(
        substituted_source
            .read("workspace-file", "file-public", Some(&claim))
            .await
            .is_err(),
        "F5"
    );

    let missing_digest = axum::Router::new().route(
        "/v1/worker/resources/files/content",
        axum::routing::post(|| async { b"unidentified-file".to_vec() }),
    );
    let missing_digest_address = support::serve(missing_digest).await;
    let missing_digest_source = HttpFileContentSource::new(
        WorkerUpstream::new(format!("http://{missing_digest_address}"))
            .with_worker_id("worker-file"),
    );
    let missing_digest_error = missing_digest_source
        .read("workspace-file", "file-public", Some(&claim))
        .await
        .expect_err("F7 digest header is mandatory");
    assert!(
        missing_digest_error
            .to_string()
            .contains("no content digest"),
        "F7: {missing_digest_error}"
    );

    let missing_claim = claimed_dispatch(&dispatch, "file-missing", &identity.lease_owner()).await;
    assert_eq!(
        source
            .read("workspace-file", "file-missing", Some(&missing_claim))
            .await
            .expect("F8 not-found response"),
        None,
        "F8"
    );
    dispatch
        .settle(
            &missing_claim.run_id,
            missing_claim.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();

    store
        .create_file(awaken_resource_contract::FileRecord {
            id: "file-broken".into(),
            workspace_id: "workspace-file".into(),
            blob_id: awaken_resource_contract::content_id(b"missing-blob"),
            filename: "broken.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 12,
            created_at: "2026-07-30T00:00:00Z".into(),
            downloadable: false,
            scope_id: None,
            logical_path: None,
            harvest_key: None,
            deleted: false,
        })
        .await
        .unwrap();
    let broken_claim = claimed_dispatch(&dispatch, "file-broken", &identity.lease_owner()).await;
    let broken = source
        .read("workspace-file", "file-broken", Some(&broken_claim))
        .await
        .expect_err("F9 missing blob must fail closed");
    assert!(broken.to_string().contains("503"), "F9: {broken}");
}

/// An application contribution can freeze its Session Resource generation only
/// after the durable Run was enqueued. The Coordinator must therefore authorize
/// that exact generation from the durable Session aggregate, while preserving
/// the same claim, Workspace, Worker-incarnation, realization-lease, and File
/// fences used by an inline dispatch envelope.
#[tokio::test]
async fn application_session_file_content_uses_its_claimed_frozen_generation() {
    let store = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let digest = store.put(b"application-file").await.unwrap();
    store
        .create_file(awaken_resource_contract::FileRecord {
            id: "file-application".into(),
            workspace_id: "workspace-file".into(),
            blob_id: digest.clone(),
            filename: "input.bin".into(),
            mime_type: "application/octet-stream".into(),
            size_bytes: 16,
            created_at: "2026-08-10T00:00:00Z".into(),
            downloadable: false,
            scope_id: None,
            logical_path: None,
            harvest_key: None,
            deleted: false,
        })
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-application-file").await;
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let claim = claimed_application_dispatch(
        &dispatch,
        &identity.lease_owner(),
        "session-application-file",
    )
    .await;
    let sessions =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let session =
        frozen_application_session("session-application-file", "file-application", &identity);
    sessions
        .create(
            "workspace-file",
            session,
            awaken_session_contract::IdempotencyRecord {
                key: "create:session-application-file".into(),
                payload_hash: "payload:session-application-file".into(),
            },
            Vec::new(),
        )
        .await
        .unwrap();
    let lifecycle = Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        store.clone(),
        store,
        lifecycle,
    ));
    let service = Arc::new(
        WorkerFileContentService::new(
            Arc::new(ApplicationFileContentSource::new(application)),
            dispatch,
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory)
        .with_application_sessions(sessions),
    );
    let address = support::serve(worker_file_content_router(service)).await;
    let source = HttpFileContentSource::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    );

    let exact = source
        .read("workspace-file", "file-application", Some(&claim))
        .await
        .expect("frozen application File read")
        .expect("existing application File");
    assert_eq!(exact, (digest, b"application-file".to_vec()));

    let unfrozen = source
        .read("workspace-file", "file-other", Some(&claim))
        .await
        .expect_err("a File outside the frozen generation must be denied");
    assert!(unfrozen.to_string().contains("403"), "{unfrozen}");
}

/// Cause/effect rationale: a remote source without the exact claim has no
/// authority to contact the File data plane, so it fails locally (C1 -> E1) and
/// cannot accidentally become an unfenced compatibility path.
#[tokio::test]
async fn remote_file_source_requires_claim() {
    let source = HttpFileContentSource::new(WorkerUpstream::new("http://127.0.0.1:1"));
    let error = source
        .read("workspace-file", "file-public", None)
        .await
        .expect_err("claim is mandatory");
    assert!(error.to_string().contains("requires a dispatch claim"));

    let claim = RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("run-unused".into()),
        owner: "worker-unused".into(),
        epoch: 1,
    };
    let empty = source
        .read("", "file-public", Some(&claim))
        .await
        .expect_err("empty Workspace is rejected locally");
    assert!(empty.to_string().contains("must not be empty"));
}
