//! File Resource Coordinator boundary tests over real HTTP.

use awaken_run_ingress_testkit::worker_http as support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_file_store::FileStore as _;
use awaken_resource_application::ApplicationFileContentSource;
use awaken_resource_contract::FileCatalog as _;
use awaken_resource_contract::FileContentSource as _;
use awaken_resource_contract::{FileReadPurpose, ResolvedFileContent};
use awaken_resource_worker_http::HttpFileContentSource;
use awaken_resource_worker_http::{WorkerFileContentService, worker_file_content_router};
use awaken_run_ingress::{
    DispatchQueue as _, MemoryDispatchStore, RunClaim, RunDispatch, WorkerIdentity,
};
use awaken_session_contract::ManagedSessionRepository as _;
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

fn resources(file_id: &str) -> awaken_session_contract::ResolvedSessionResources {
    awaken_session_contract::ResolvedSessionResources::try_new(
        vec![awaken_session_contract::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::new("file-binding"),
            source: awaken_session_contract::ResolvedInputSource::File {
                file_id: awaken_resource_contract::FileId::from(file_id),
            },
            mount_path: "/workspace/input.txt".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }],
        Vec::new(),
    )
    .unwrap()
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

async fn claimed_model_dispatch(
    dispatch: &Arc<MemoryDispatchStore>,
    file_id: &str,
    owner: &str,
) -> RunClaim {
    let mut activation = support::activation("model-content");
    activation.input = vec![awaken_agent_contract::agent::message::Message::new(
        awaken_agent_contract::agent::message::Id("message-model-content".into()),
        awaken_agent_contract::agent::message::Role::User,
        vec![
            awaken_agent_contract::agent::content::ContentBlock::tool_result(
                "call-model-content",
                vec![awaken_agent_contract::agent::content::ContentBlock::document_file(file_id)],
            ),
        ],
    )];
    let request = RunDispatch::new(activation).with_execution_scope(
        awaken_tenancy::ExecutionScopeRef(awaken_tenancy::ScopeId::from("workspace-file")),
    );
    dispatch.enqueue(request).await.unwrap();
    let claimed = dispatch
        .claim(owner, 60_000, support::unix_now_ms(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    RunClaim::from(&claimed.lease)
}

struct RecordingParentRecovery {
    expected_session_thread_id: ThreadId,
    expected_thread_id: ThreadId,
    expected_run_id: RunId,
    snapshot: RunRecoverySnapshot,
    legacy_calls: AtomicUsize,
    in_session_calls: Mutex<Vec<(ThreadId, ThreadId, RunId)>>,
}

#[async_trait::async_trait]
impl RunRecoverySource for RecordingParentRecovery {
    async fn recovery_snapshot(
        &self,
        _thread_id: &ThreadId,
        _claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.legacy_calls.fetch_add(1, Ordering::SeqCst);
        Err(RecoveryError::Rejected(
            "legacy logical-only recovery must not authorize a child File read".into(),
        ))
    }

    async fn recovery_snapshot_in_session(
        &self,
        session_thread_id: &ThreadId,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.in_session_calls.lock().unwrap().push((
            session_thread_id.clone(),
            thread_id.clone(),
            claimed_run_id.clone(),
        ));
        if session_thread_id != &self.expected_session_thread_id
            || thread_id != &self.expected_thread_id
            || claimed_run_id != &self.expected_run_id
        {
            return Err(RecoveryError::Rejected(
                "File recovery coordinates do not match the guarded child dispatch".into(),
            ));
        }
        Ok(self.snapshot.clone())
    }
}

async fn claimed_recovery_model_dispatch(
    dispatch: &Arc<MemoryDispatchStore>,
    owner: &str,
) -> (RunClaim, ThreadId, ThreadId, RunId) {
    let mut activation = support::activation("model-content-recovery-child");
    activation.input.clear();
    let thread_id = activation.thread_id.clone();
    let run_id = activation.run_id.clone();
    let session_thread_id = ThreadId("session-model-content-recovery-parent".into());
    dispatch
        .enqueue(
            RunDispatch::new(activation)
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from("workspace-file"),
                ))
                .for_session(session_thread_id.clone()),
        )
        .await
        .unwrap();
    let claimed = dispatch
        .claim(owner, 60_000, support::unix_now_ms(), &Default::default())
        .await
        .unwrap()
        .expect("recovery-only child dispatch claim");
    (
        RunClaim::from(&claimed.lease),
        session_thread_id,
        thread_id,
        run_id,
    )
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
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    let mut resource_state = awaken_session_contract::SessionResourceState::default();
    resource_state
        .prepare(session_id, resources(file_id))
        .expect("prepare Worker Session resources");
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
                    agent_revision: None,
                    model_override: None,
                    model: "model".into(),
                    runtime: None,
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                    transcript_prefix: None,
                },
            ),
        ),
        title: None,
        metadata: Default::default(),
        tools: Default::default(),
        budget: Default::default(),
        event_batches: Vec::new(),
        activity_epoch: 0,
        active_activity_epochs: Default::default(),
        running_interval: None,
        runtime_active_millis: 0,
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
        .read(
            "workspace-file",
            "file-public",
            &FileReadPurpose::SessionResource,
            Some(&claim),
        )
        .await
        .expect("F1 exact File read")
        .expect("F1 existing File");
    assert_eq!(
        exact,
        ResolvedFileContent {
            file_id: "file-public".into(),
            content_id: digest.clone(),
            filename: "input.txt".into(),
            media_type: "text/plain".into(),
            bytes: b"exact-file".to_vec(),
        },
        "F1"
    );

    let denied = source
        .read(
            "workspace-file",
            "file-other",
            &FileReadPurpose::SessionResource,
            Some(&claim),
        )
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
        .read(
            "workspace-file",
            "file-public",
            &FileReadPurpose::SessionResource,
            Some(&claim),
        )
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
        .read(
            "workspace-file",
            "file-public",
            &FileReadPurpose::SessionResource,
            Some(&claim),
        )
        .await
        .expect_err("F4 stale claim must be rejected");
    assert!(stale.to_string().contains("409"), "F4: {stale}");

    use base64::Engine as _;
    let substituted_metadata = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            "file_id": "file-public",
            "content_id": digest,
            "filename": "input.txt",
            "media_type": "text/plain"
        }))
        .unwrap(),
    );
    let substituted = axum::Router::new().route(
        "/v1/worker/resources/files/content",
        axum::routing::post(move || async move {
            (
                [("x-awaken-file-content-metadata", substituted_metadata)],
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
            .read(
                "workspace-file",
                "file-public",
                &FileReadPurpose::SessionResource,
                Some(&claim),
            )
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
        .read(
            "workspace-file",
            "file-public",
            &FileReadPurpose::SessionResource,
            Some(&claim),
        )
        .await
        .expect_err("F7 digest header is mandatory");
    assert!(
        missing_digest_error
            .to_string()
            .contains("no content metadata"),
        "F7: {missing_digest_error}"
    );

    let missing_claim = claimed_dispatch(&dispatch, "file-missing", &identity.lease_owner()).await;
    assert_eq!(
        source
            .read(
                "workspace-file",
                "file-missing",
                &FileReadPurpose::SessionResource,
                Some(&missing_claim),
            )
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
        .read(
            "workspace-file",
            "file-broken",
            &FileReadPurpose::SessionResource,
            Some(&broken_claim),
        )
        .await
        .expect_err("F9 missing blob must fail closed");
    assert!(broken.to_string().contains("503"), "F9: {broken}");
}

/// A Worker Session authorizes its exact frozen Resource generation from the
/// durable Session aggregate, while preserving
/// the same claim, Workspace, Worker-incarnation, realization-lease, and File
/// fences used by an inline dispatch envelope.
#[tokio::test]
async fn worker_session_file_content_uses_its_claimed_frozen_generation() {
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
        .with_session_repository(sessions),
    );
    let address = support::serve(worker_file_content_router(service)).await;
    let source = HttpFileContentSource::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    );

    let exact = source
        .read(
            "workspace-file",
            "file-application",
            &FileReadPurpose::SessionResource,
            Some(&claim),
        )
        .await
        .expect("frozen application File read")
        .expect("existing application File");
    assert_eq!(
        exact,
        ResolvedFileContent {
            file_id: "file-application".into(),
            content_id: digest,
            filename: "input.bin".into(),
            media_type: "application/octet-stream".into(),
            bytes: b"application-file".to_vec(),
        }
    );

    let unfrozen = source
        .read(
            "workspace-file",
            "file-other",
            &FileReadPurpose::SessionResource,
            Some(&claim),
        )
        .await
        .expect_err("a File outside the frozen generation must be denied");
    assert!(unfrozen.to_string().contains("403"), "{unfrozen}");
}

/// Model-content authorization cause graph and FMECA:
/// C1=live exact claim; C2=request thread equals activation thread; C3=File is
/// strongly referenced in activation input (including nested ToolResult); C4=
/// Workspace matches dispatch scope. All C1-C4 -> return verified immutable
/// content (rule M1). !C2 -> deny (M2); !C3 -> deny before catalog read (M3).
/// Existing transport rules own !C1/!C4. FMECA: accepting a caller-supplied File
/// id without a trusted transcript edge is an authority escalation (critical;
/// strong recursive reference check); trusting a foreign thread is cross-run
/// disclosure (critical; exact activation-thread equality).
#[tokio::test]
async fn model_content_file_requires_its_claimed_thread_and_strong_input_reference() {
    let store = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let digest = store.put(b"model-file").await.unwrap();
    store
        .create_file(awaken_resource_contract::FileRecord {
            id: "file-model".into(),
            workspace_id: "workspace-file".into(),
            blob_id: digest,
            filename: "model.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 10,
            created_at: "2026-08-16T00:00:00Z".into(),
            downloadable: false,
            scope_id: None,
            logical_path: None,
            harvest_key: None,
            deleted: false,
        })
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-model-content").await;
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let claim = claimed_model_dispatch(&dispatch, "file-model", &identity.lease_owner()).await;
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
        .with_worker_directory(directory),
    );
    let address = support::serve(worker_file_content_router(service)).await;
    let source = HttpFileContentSource::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    );
    let exact = source
        .read(
            "workspace-file",
            "file-model",
            &FileReadPurpose::ModelContent {
                thread_id: "thread-model-content".into(),
            },
            Some(&claim),
        )
        .await
        .expect("M1 authorized read")
        .expect("M1 existing file");
    assert_eq!(exact.bytes, b"model-file", "M1");

    let wrong_thread = source
        .read(
            "workspace-file",
            "file-model",
            &FileReadPurpose::ModelContent {
                thread_id: "thread-foreign".into(),
            },
            Some(&claim),
        )
        .await
        .expect_err("M2 foreign thread");
    assert!(wrong_thread.to_string().contains("403"), "M2");

    let unreferenced = source
        .read(
            "workspace-file",
            "file-other",
            &FileReadPurpose::ModelContent {
                thread_id: "thread-model-content".into(),
            },
            Some(&claim),
        )
        .await
        .expect_err("M3 unreferenced File");
    assert!(unreferenced.to_string().contains("403"), "M3");
}

#[tokio::test]
async fn model_content_recovery_uses_parent_partition_for_a_child_thread() {
    // Test design.
    // C: C1 a live claimed child has a parent physical Session distinct from its
    // logical Thread; C2 activation input has no File reference; C3 the exact
    // committed child transcript references the File; C4 the legacy logical-only
    // recovery method is an injected failure; C5 the physical-aware method
    // receives the guarded parent/logical/Run tuple.
    // E: E1 C1-C3+C5 authorizes and returns the immutable File bytes; E2 the
    // legacy method is never called; E3 exactly one physical-aware read records
    // the parent Session, child Thread, and claimed Run without inferring a child
    // physical partition.
    // K: the live dispatch guard, not the caller's ModelContent payload, owns all
    // recovery coordinates; the recovered transcript remains the only authority
    // for a File absent from activation input.
    // D: R1=(C1,C2,C3,C5 with C4 not invoked)=>E1-E3. C4 is the negative
    // oracle: selecting that compatibility path fails closed before File return.
    let store = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let digest = store.put(b"recovered-model-file").await.unwrap();
    store
        .create_file(awaken_resource_contract::FileRecord {
            id: "file-recovered-model".into(),
            workspace_id: "workspace-file".into(),
            blob_id: digest,
            filename: "recovered-model.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 20,
            created_at: "2026-08-23T00:00:00Z".into(),
            downloadable: false,
            scope_id: None,
            logical_path: None,
            harvest_key: None,
            deleted: false,
        })
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-model-recovery").await;
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let (claim, session_thread_id, thread_id, run_id) =
        claimed_recovery_model_dispatch(&dispatch, &identity.lease_owner()).await;
    let recovery = Arc::new(RecordingParentRecovery {
        expected_session_thread_id: session_thread_id.clone(),
        expected_thread_id: thread_id.clone(),
        expected_run_id: run_id.clone(),
        snapshot: RunRecoverySnapshot {
            thread_id: thread_id.clone(),
            claimed_run_id: run_id.clone(),
            runs: vec![RunRecord {
                id: run_id.clone(),
                thread_id: thread_id.clone(),
                state: RunState::Running,
            }],
            latest_run_id: Some(run_id.clone()),
            messages: vec![Message::new(
                MessageId("message-recovered-model".into()),
                Role::User,
                vec![
                    awaken_agent_contract::agent::content::ContentBlock::document_file(
                        "file-recovered-model",
                    ),
                ],
            )],
            state: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 0,
        },
        legacy_calls: AtomicUsize::new(0),
        in_session_calls: Mutex::new(Vec::new()),
    });
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
        .with_recovery(recovery.clone()),
    );
    let address = support::serve(worker_file_content_router(service)).await;
    let source = HttpFileContentSource::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    );

    let exact = source
        .read(
            "workspace-file",
            "file-recovered-model",
            &FileReadPurpose::ModelContent {
                thread_id: thread_id.0.clone(),
            },
            Some(&claim),
        )
        .await
        .expect("R1/E1 recovered child File read")
        .expect("R1/E1 existing recovered File");
    assert_eq!(exact.bytes, b"recovered-model-file", "R1/E1");
    assert_eq!(recovery.legacy_calls.load(Ordering::SeqCst), 0, "R1/E2");
    assert_eq!(
        recovery.in_session_calls.lock().unwrap().as_slice(),
        [(session_thread_id, thread_id, run_id)],
        "R1/E3"
    );
}

/// Cause/effect rationale: a remote source without the exact claim has no
/// authority to contact the File data plane, so it fails locally (C1 -> E1) and
/// cannot accidentally become an unfenced compatibility path.
#[tokio::test]
async fn remote_file_source_requires_claim() {
    let source = HttpFileContentSource::new(WorkerUpstream::new("http://127.0.0.1:1"));
    let error = source
        .read(
            "workspace-file",
            "file-public",
            &FileReadPurpose::SessionResource,
            None,
        )
        .await
        .expect_err("claim is mandatory");
    assert!(error.to_string().contains("requires a dispatch claim"));

    let claim = RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("run-unused".into()),
        owner: "worker-unused".into(),
        epoch: 1,
    };
    let empty = source
        .read(
            "",
            "file-public",
            &FileReadPurpose::SessionResource,
            Some(&claim),
        )
        .await
        .expect_err("empty Workspace is rejected locally");
    assert!(empty.to_string().contains("must not be empty"));
}
