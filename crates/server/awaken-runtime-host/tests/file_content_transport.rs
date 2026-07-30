//! File Resource boundary tests over real HTTP.

use std::sync::Arc;

use awaken_file_store::FileStore as _;
use awaken_resource_contract::FileCatalog as _;
use awaken_run_ingress::{
    DispatchQueue as _, MemoryDispatchStore, RegisteredWorker, RegistryError, RegistryMutation,
    RunClaim, RunDispatch, WorkerDirectory, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerRegistration, WorkerSnapshot, WorkerState,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{
    FileContentSource as _, HeaderWorkerAuthenticator, HttpFileContentSource,
    StoreFileContentSource, WorkerFileContentService, WorkerUpstream, worker_file_content_router,
};

async fn serve(app: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

struct CurrentWorkerDirectory(RegisteredWorker);

#[async_trait::async_trait]
impl WorkerDirectory for CurrentWorkerDirectory {
    async fn register(
        &self,
        _registration: WorkerRegistration,
        _now_ms: u64,
        _ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        Ok(self.0.clone())
    }

    async fn heartbeat(
        &self,
        _identity: &WorkerIdentity,
        _heartbeat: WorkerHeartbeat,
        _now_ms: u64,
        _ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn begin_drain(
        &self,
        _identity: &WorkerIdentity,
        _deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn mark_quiesced(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn deregister(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok((worker_id == self.0.snapshot.identity.worker_id).then(|| self.0.clone()))
    }

    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(vec![self.0.clone()])
    }

    async fn expire(&self, _now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError> {
        Ok(Vec::new())
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn activation() -> RunActivation {
    RunActivation::new(
        awaken_agent_contract::agent::run::Id("run-file".into()),
        awaken_agent_contract::agent::thread::Id("thread-file".into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-file".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-file".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("catalog-file".into()),
                instructions: "read the File".into(),
                max_steps: 1,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("test", "model", "native"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("snapshot-file-fingerprint".into()),
        },
        Vec::new(),
    )
}

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
    let request = RunDispatch::new(activation())
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-file"),
        ))
        .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
            "workspace-file",
            serde_json::to_string(&resources(file_id)).unwrap(),
        ));
    dispatch.enqueue(request).await.unwrap();
    let claimed = dispatch
        .claim(owner, 60_000, unix_now_ms(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    RunClaim::from(&claimed.lease)
}

/// Cause/effect decision table:
/// | Rule | Worker auth | live exact claim | Workspace/File frozen | stored digest | Effect |
/// |---|---|---|---|---|---|
/// | F1 | valid | yes | yes | exact | return immutable bytes and digest |
/// | F2 | valid | yes | another File | any | deny before File source |
/// | F3 | missing | any | any | any | HTTP 401 before claim/store |
/// | F4 | valid | stale | yes | any | reject without content |
/// | F5 | valid | yes | yes | substituted response | Worker rejects bytes |
/// | F6 | stale incarnation | yes | yes | exact | deny before File source |
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
    let identity = WorkerIdentity::new("worker-file", "worker-file-boot", 1);
    let manifest = WorkerManifest::default();
    let directory = Arc::new(CurrentWorkerDirectory(RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: identity.clone(),
            state: WorkerState::Ready,
            capability_fingerprint: manifest.fingerprint().unwrap(),
            manifest,
            in_flight: 0,
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms: u64::MAX,
        },
        heartbeat_sequence: 0,
        registered_at_ms: 0,
        heartbeat_at_ms: 0,
        drain_deadline_ms: None,
    }));
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let claim = claimed_dispatch(&dispatch, "file-public", &identity.lease_owner()).await;
    let source = Arc::new(StoreFileContentSource::new(store.clone(), store));
    let service = Arc::new(
        WorkerFileContentService::new(
            source,
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory),
    );
    let address = serve(worker_file_content_router(service)).await;
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
    let substituted_address = serve(substituted).await;
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
}
