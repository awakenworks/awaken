//! Session artifact publication over the real Worker HTTP boundary.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublisher as _, ArtifactRecovery, FileApplicationService,
    FileRecord, FileStore, FileStoreError, ResourcePurgeError, content_id,
};
use awaken_resource_worker_http::{
    ARTIFACT_METADATA_HEADER, ARTIFACT_PUBLICATION_PATH, HttpArtifactPublisher,
    WorkerArtifactPublicationService, worker_artifact_publication_router,
};
use awaken_run_ingress::{
    ArtifactPublicationFence, DispatchQueue as _, MemoryDispatchStore, RunClaim, RunDispatch,
    WorkerIdentity,
};
use awaken_run_ingress_testkit::worker_http as support;
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};
use base64::Engine as _;

async fn claimed_dispatch(
    dispatch: &Arc<MemoryDispatchStore>,
    session: &str,
    owner: &str,
) -> RunClaim {
    let request = RunDispatch::new(support::activation(session)).with_execution_scope(
        awaken_tenancy::ExecutionScopeRef(awaken_tenancy::ScopeId::from("workspace-artifacts")),
    );
    dispatch.enqueue(request).await.unwrap();
    let claimed = dispatch
        .claim(owner, 60_000, support::unix_now_ms(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    RunClaim::from(&claimed.lease)
}

fn publication(
    claim: Option<RunClaim>,
    workspace: &str,
    session: &str,
    path: &str,
    bytes: &[u8],
) -> ArtifactPublication<ArtifactPublicationFence> {
    let content_id = awaken_resource_contract::content_id(bytes);
    ArtifactPublication {
        effect_id: awaken_resource_contract::harvest_idempotency_key(session, path, &content_id),
        workspace_id: workspace.into(),
        session_id: session.into(),
        logical_path: path.into(),
        mime_type: "text/html".into(),
        content_id,
        bytes: bytes.to_vec(),
        idempotency_scope: None,
        fence: claim.map(ArtifactPublicationFence::Run),
    }
}

fn terminal_publication(
    workspace: &str,
    session: &str,
    path: &str,
    bytes: &[u8],
    effect: awaken_session_contract::SessionTerminalCleanupEffect,
) -> ArtifactPublication<ArtifactPublicationFence> {
    let content_id = content_id(bytes);
    ArtifactPublication {
        effect_id: awaken_resource_contract::harvest_idempotency_key(session, path, &content_id),
        workspace_id: workspace.into(),
        session_id: session.into(),
        logical_path: path.into(),
        mime_type: "text/plain".into(),
        content_id,
        bytes: bytes.to_vec(),
        idempotency_scope: Some(effect.operation_id().to_string()),
        fence: Some(ArtifactPublicationFence::Terminal(effect)),
    }
}

fn checkpoint_release_publication(
    workspace: &str,
    session: &str,
    path: &str,
    bytes: &[u8],
    operation: awaken_session_contract::SessionEnvironmentOperation,
) -> ArtifactPublication<ArtifactPublicationFence> {
    let content_id = content_id(bytes);
    ArtifactPublication {
        effect_id: awaken_resource_contract::harvest_idempotency_key(session, path, &content_id),
        workspace_id: workspace.into(),
        session_id: session.into(),
        logical_path: path.into(),
        mime_type: "text/plain".into(),
        content_id,
        bytes: bytes.to_vec(),
        idempotency_scope: None,
        fence: Some(ArtifactPublicationFence::CheckpointRelease(operation)),
    }
}

fn metadata_header(
    claim: &RunClaim,
    identity: &WorkerIdentity,
    workspace: &str,
    session: &str,
    path: &str,
    digest: &str,
) -> String {
    let effect_id = awaken_resource_contract::harvest_idempotency_key(session, path, digest);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            "claim": claim,
            "identity": identity,
            "workspace_id": workspace,
            "session_id": session,
            "logical_path": path,
            "mime_type": "text/html",
            "content_id": digest,
            "effect_id": effect_id,
            "idempotency_scope": null,
        }))
        .unwrap(),
    )
}

struct FailingFileStore;

struct ExactArtifactControl {
    workspace_id: String,
    terminal_effect: Option<awaken_session_contract::SessionTerminalCleanupEffect>,
    checkpoint: Option<(String, awaken_session_contract::SessionEnvironmentOperation)>,
}

struct CountingFileApplication {
    inner: Arc<dyn FileApplicationService>,
    recovery_lists: AtomicUsize,
    artifact_creates: AtomicUsize,
}

impl CountingFileApplication {
    fn new(inner: Arc<dyn FileApplicationService>) -> Self {
        Self {
            inner,
            recovery_lists: AtomicUsize::new(0),
            artifact_creates: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl FileApplicationService for CountingFileApplication {
    async fn get(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        self.inner.get(workspace_id, file_id).await
    }

    async fn list(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError> {
        self.inner.list(workspace_id, scope_id).await
    }

    async fn list_including_deleted(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError> {
        self.recovery_lists.fetch_add(1, Ordering::SeqCst);
        self.inner
            .list_including_deleted(workspace_id, scope_id)
            .await
    }

    async fn create_uploaded_file_with_expiry_and_idempotency(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        expires_at: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.inner
            .create_uploaded_file_with_expiry_and_idempotency(
                workspace_id,
                filename,
                mime_type,
                bytes,
                expires_at,
                idempotency_key,
            )
            .await
    }

    async fn create_generated_file(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        idempotency_key: String,
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.inner
            .create_generated_file(workspace_id, filename, mime_type, bytes, idempotency_key)
            .await
    }

    async fn create_artifact(
        &self,
        publication: &ArtifactPublication<()>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.artifact_creates.fetch_add(1, Ordering::SeqCst);
        self.inner.create_artifact(publication).await
    }

    async fn bytes(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<(FileRecord, Vec<u8>)>, ResourcePurgeError> {
        self.inner.bytes(workspace_id, file_id).await
    }

    async fn delete(
        &self,
        workspace_id: &str,
        file_id: &str,
        requested_at_unix_ms: u64,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        self.inner
            .delete(workspace_id, file_id, requested_at_unix_ms)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRealizationControl for ExactArtifactControl {
    async fn begin_session_realization(
        &self,
        _command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        Err(
            awaken_session_contract::SessionRealizationControlFailure::Invalid(
                "test control has no realization driver".into(),
            ),
        )
    }

    async fn activate_session_realization(
        &self,
        _command: awaken_session_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        Err(
            awaken_session_contract::SessionRealizationControlFailure::Invalid(
                "test control has no realization driver".into(),
            ),
        )
    }

    async fn acknowledge_session_realization(
        &self,
        _command: awaken_session_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        Err(
            awaken_session_contract::SessionRealizationControlFailure::Invalid(
                "test control has no realization driver".into(),
            ),
        )
    }

    async fn fail_session_realization(
        &self,
        _command: awaken_session_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        Err(
            awaken_session_contract::SessionRealizationControlFailure::Invalid(
                "test control has no realization driver".into(),
            ),
        )
    }

    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        let Some(current) = self.terminal_effect.as_ref() else {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        };
        if current.command != effect.command
            || !awaken_session_contract::realization_lease_generation_authorizes(
                &current.lease,
                &effect.lease,
            )
            || !awaken_session_contract::realization_lease_is_live_at(
                current.lease.expires_at_unix_ms,
                support::unix_now_ms(),
            )
        {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            effect.clone(),
            self.workspace_id.clone(),
            None,
        )
    }

    async fn authorize_checkpoint_release_artifact_effect(
        &self,
        session_id: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
    ) -> Result<String, awaken_session_contract::SessionRealizationControlFailure> {
        if !self
            .checkpoint
            .as_ref()
            .is_some_and(|(expected_session, expected_operation)| {
                expected_session == session_id && expected_operation == operation
            })
        {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        Ok(self.workspace_id.clone())
    }
}

#[async_trait::async_trait]
impl FileStore for FailingFileStore {
    async fn put(&self, _bytes: &[u8]) -> Result<String, FileStoreError> {
        Err(FileStoreError("injected storage outage".into()))
    }

    async fn get(&self, _id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        Ok(None)
    }

    async fn list(&self) -> Result<Vec<String>, FileStoreError> {
        Ok(Vec::new())
    }

    async fn delete(&self, _id: &str) -> Result<bool, FileStoreError> {
        Ok(false)
    }
}

/// Artifact-publication FMECA and cause/effect decision table:
///
/// | Rule | Worker auth/incarnation | exact live claim | scope + Session | path/digest | Resource app | Effect |
/// |---|---|---|---|---|---|---|
/// | A1 | valid | yes | exact | valid | available | publish exact scoped File |
/// | A2 | valid | same | exact | same bytes | available | idempotently return same File |
/// | A3 | valid | same | exact | changed bytes | available | create immutable new version |
/// | A4 | missing | any | any | any | any | 401 before claim/application |
/// | A5 | valid | stale | exact | valid | available | 409; publish nothing |
/// | A6 | valid | yes | foreign Workspace/Session | valid | available | 403; publish nothing |
/// | A7 | valid | yes | exact | substituted digest | available | 400; publish nothing |
/// | A8 | stale incarnation | yes | exact | valid | available | 403; publish nothing |
/// | A9 | valid | yes | exact | traversal path | available | 400; publish nothing |
/// | A10 | valid | yes | exact | valid | unavailable | 503; claim remains unsettled |
/// | A11 | valid | absent | exact | valid | available | Worker client fails closed pre-I/O |
/// | A12 | valid | malformed metadata | any | any | available | 400; publish nothing |
///
/// Failure modes mitigated: stale-attempt write, cross-scope publication,
/// content substitution, path traversal, replay duplication, and false success
/// during Resource outage. The commit-epoch guard remains held across A1-A3/A10.
#[tokio::test]
async fn artifact_publication_is_claim_fenced_digest_verified_and_idempotent() {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let lifecycle = Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        files.clone(),
        files,
        lifecycle.clone(),
    ));
    let (directory, identity) = support::ready_worker("worker-artifacts").await;
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let claim = claimed_dispatch(&dispatch, "session-artifacts", &identity.lease_owner()).await;
    let service = Arc::new(WorkerArtifactPublicationService::new(
        application.clone(),
        dispatch.clone(),
        Arc::new(HeaderWorkerAuthenticator),
        directory.clone(),
    ));
    let address = support::serve(worker_artifact_publication_router(service)).await;
    let upstream =
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone());
    let publisher = HttpArtifactPublisher::new(upstream.clone());

    let first = publisher
        .publish(publication(
            Some(claim.clone()),
            "workspace-artifacts",
            "thread-session-artifacts",
            "prototype/index.html",
            b"revision one",
        ))
        .await
        .expect("A1 exact artifact");
    assert_eq!(
        first.record.scope_id.as_deref(),
        Some("thread-session-artifacts"),
        "A1"
    );
    assert_eq!(
        first.record.logical_path.as_deref(),
        Some("prototype/index.html"),
        "A1"
    );

    let retry = publisher
        .publish(publication(
            Some(claim.clone()),
            "workspace-artifacts",
            "thread-session-artifacts",
            "prototype/index.html",
            b"revision one",
        ))
        .await
        .expect("A2 retry");
    assert_eq!(retry.record.id, first.record.id, "A2");

    let changed = publisher
        .publish(publication(
            Some(claim.clone()),
            "workspace-artifacts",
            "thread-session-artifacts",
            "prototype/index.html",
            b"revision two",
        ))
        .await
        .expect("A3 changed bytes");
    assert_ne!(changed.record.id, first.record.id, "A3");
    assert_eq!(
        application
            .list("workspace-artifacts", Some("thread-session-artifacts"))
            .await
            .unwrap()
            .len(),
        2,
        "A1-A3"
    );

    let unauthenticated = reqwest::Client::new()
        .post(format!("http://{address}{ARTIFACT_PUBLICATION_PATH}"))
        .header(
            ARTIFACT_METADATA_HEADER,
            metadata_header(
                &claim,
                &identity,
                "workspace-artifacts",
                "thread-session-artifacts",
                "unauthenticated.html",
                &content_id(b"x"),
            ),
        )
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauthenticated.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "A4"
    );

    let malformed = upstream
        .authorize_request(
            "POST",
            ARTIFACT_PUBLICATION_PATH,
            upstream
                .http_client()
                .post(format!("http://{address}{ARTIFACT_PUBLICATION_PATH}"))
                .header(ARTIFACT_METADATA_HEADER, "not-base64")
                .body("x"),
        )
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST, "A12");

    for (workspace, session, path, rule, status) in [
        (
            "workspace-foreign",
            "thread-session-artifacts",
            "scope.html",
            "A6",
            "403",
        ),
        (
            "workspace-artifacts",
            "session-foreign",
            "session.html",
            "A6",
            "403",
        ),
        (
            "workspace-artifacts",
            "thread-session-artifacts",
            "../escape.html",
            "A9",
            "400",
        ),
    ] {
        let error = publisher
            .publish(publication(
                Some(claim.clone()),
                workspace,
                session,
                path,
                b"denied",
            ))
            .await
            .expect_err(rule);
        assert!(error.to_string().contains(status), "{rule}: {error}");
    }

    let substituted_request = upstream
        .http_client()
        .post(format!("http://{address}{ARTIFACT_PUBLICATION_PATH}"))
        .header(
            ARTIFACT_METADATA_HEADER,
            metadata_header(
                &claim,
                &identity,
                "workspace-artifacts",
                "thread-session-artifacts",
                "substituted.html",
                &content_id(b"expected"),
            ),
        )
        .body("substituted");
    let substituted_request = upstream
        .authorize_request("POST", ARTIFACT_PUBLICATION_PATH, substituted_request)
        .unwrap();
    assert_eq!(
        substituted_request.send().await.unwrap().status(),
        reqwest::StatusCode::BAD_REQUEST,
        "A7"
    );

    let stale_publisher =
        HttpArtifactPublisher::new(
            WorkerUpstream::new(format!("http://{address}")).with_worker_identity(
                WorkerIdentity::new("worker-artifacts", "stale-incarnation", 2),
            ),
        );
    let stale_identity = stale_publisher
        .publish(publication(
            Some(claim.clone()),
            "workspace-artifacts",
            "thread-session-artifacts",
            "stale-worker.html",
            b"denied",
        ))
        .await
        .expect_err("A8");
    assert!(stale_identity.to_string().contains("403"), "A8");

    let no_claim = publisher
        .publish(publication(
            None,
            "workspace-artifacts",
            "thread-session-artifacts",
            "missing-claim.html",
            b"denied",
        ))
        .await
        .expect_err("A11");
    assert!(
        no_claim.to_string().contains("requires an execution fence"),
        "A11"
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
    let stale_claim = publisher
        .publish(publication(
            Some(claim),
            "workspace-artifacts",
            "thread-session-artifacts",
            "stale-claim.html",
            b"denied",
        ))
        .await
        .expect_err("A5");
    assert!(stale_claim.to_string().contains("409"), "A5");
    assert_eq!(
        application
            .list("workspace-artifacts", Some("thread-session-artifacts"))
            .await
            .unwrap()
            .len(),
        2,
        "A4-A9/A12 publish nothing"
    );

    let unavailable_claim =
        claimed_dispatch(&dispatch, "session-unavailable", &identity.lease_owner()).await;
    let unavailable_application = Arc::new(awaken_resource_application::FileApplication::new(
        Arc::new(FailingFileStore),
        Arc::new(awaken_file_store::InMemoryFileStore::new()),
        lifecycle,
    ));
    let unavailable = Arc::new(WorkerArtifactPublicationService::new(
        unavailable_application,
        dispatch.clone(),
        Arc::new(HeaderWorkerAuthenticator),
        directory,
    ));
    let unavailable_address = support::serve(worker_artifact_publication_router(unavailable)).await;
    let unavailable_publisher = HttpArtifactPublisher::new(
        WorkerUpstream::new(format!("http://{unavailable_address}")).with_worker_identity(identity),
    );
    let unavailable_error = unavailable_publisher
        .publish(publication(
            Some(unavailable_claim.clone()),
            "workspace-artifacts",
            "thread-session-unavailable",
            "unavailable.html",
            b"retry later",
        ))
        .await
        .expect_err("A10");
    assert!(unavailable_error.to_string().contains("503"), "A10");
    assert!(
        dispatch.lock_commit_epoch(&unavailable_claim).await.is_ok(),
        "A10: a dependency outage must not settle or invalidate the retryable claim"
    );
}

/// Terminal Artifact generation cause/effect table. The Axum body is fully
/// consumed before this authorization edge, so the asserted effect is more than
/// 20 seconds expired in every rule. Current Worker admission proves only the
/// authenticated incarnation; the existing Control root remains the sole
/// generation/current-expiry authority. The old assertion is not re-timed at
/// HTTP, while Control must prove its same generation has a live renewal.
///
/// | Rule | Worker | current root generation | current root expiry | Effect |
/// |---|---|---|---|---|
/// | T1 | current | same owner/incarnation/epoch | renewed/live | publish exact File |
/// | T2 | current | same asserted generation | not renewed/expired | HTTP 409; no File write |
/// | T3 | current | successor epoch | renewed/live | HTTP 409; no File write |
/// | T4 | foreign incarnation | any | any | HTTP 403; no root/File access |
#[tokio::test]
async fn terminal_artifact_uses_current_root_for_an_expired_asserted_generation() {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let lifecycle = Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        files.clone(),
        files,
        lifecycle,
    ));
    let (directory, identity) = support::ready_worker("worker-terminal-artifact").await;
    let now_unix_ms = support::unix_now_ms();
    let asserted = awaken_session_contract::SessionTerminalCleanupEffect::new(
        awaken_session_contract::SessionCleanupCommand::new(
            "session-terminal-artifact",
            "session-terminal-artifact",
            "cleanup-root",
        ),
        awaken_session_contract::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 4,
            expires_at_unix_ms: now_unix_ms.saturating_sub(20_001),
        },
    );
    let renewed = awaken_session_contract::SessionTerminalCleanupEffect::new(
        asserted.command.clone(),
        awaken_session_contract::SessionRealizationLease {
            expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
            ..asserted.lease.clone()
        },
    );
    let service = Arc::new(
        WorkerArtifactPublicationService::new(
            application.clone(),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
            directory.clone(),
        )
        .with_session_control(Arc::new(ExactArtifactControl {
            workspace_id: "workspace-artifacts".into(),
            terminal_effect: Some(renewed),
            checkpoint: None,
        })),
    );
    let address = support::serve(worker_artifact_publication_router(service)).await;
    let exact = HttpArtifactPublisher::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
    );

    exact
        .publish(terminal_publication(
            "workspace-artifacts",
            "session-terminal-artifact",
            "terminal/exact.txt",
            b"exact",
            asserted.clone(),
        ))
        .await
        .expect("T1 expired asserted generation is current through its renewal");

    let unrenewed_service = Arc::new(
        WorkerArtifactPublicationService::new(
            application.clone(),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
            directory,
        )
        .with_session_control(Arc::new(ExactArtifactControl {
            workspace_id: "workspace-artifacts".into(),
            terminal_effect: Some(asserted.clone()),
            checkpoint: None,
        })),
    );
    let unrenewed_address =
        support::serve(worker_artifact_publication_router(unrenewed_service)).await;
    let unrenewed = HttpArtifactPublisher::new(
        WorkerUpstream::new(format!("http://{unrenewed_address}"))
            .with_worker_identity(identity.clone()),
    );
    let error = unrenewed
        .publish(terminal_publication(
            "workspace-artifacts",
            "session-terminal-artifact",
            "terminal/unrenewed.txt",
            b"must-not-publish",
            asserted.clone(),
        ))
        .await
        .expect_err("T2 expired current root has no authority");
    assert!(error.to_string().contains("409"), "T2: {error}");

    let mut successor = asserted.clone();
    successor.lease.epoch += 1;
    successor.lease.expires_at_unix_ms = now_unix_ms.saturating_add(30_000);
    let error = exact
        .publish(terminal_publication(
            "workspace-artifacts",
            "session-terminal-artifact",
            "terminal/successor.txt",
            b"must-not-publish",
            successor,
        ))
        .await
        .expect_err("T3 a foreign epoch cannot use the renewed predecessor");
    assert!(error.to_string().contains("409"), "T3: {error}");

    let foreign =
        HttpArtifactPublisher::new(
            WorkerUpstream::new(format!("http://{address}")).with_worker_identity(
                WorkerIdentity::new("foreign-terminal-artifact", "foreign-boot", 1),
            ),
        );
    let error = foreign
        .publish(terminal_publication(
            "workspace-artifacts",
            "session-terminal-artifact",
            "terminal/foreign.txt",
            b"must-not-publish",
            asserted,
        ))
        .await
        .expect_err("T4 foreign terminal Worker");
    assert!(error.to_string().contains("403"), "T4: {error}");

    assert_eq!(
        application
            .list("workspace-artifacts", Some("session-terminal-artifact"))
            .await
            .unwrap()
            .len(),
        1,
        "T2-T4 reject before File mutation"
    );
}

/// CheckpointRelease Artifact cause/effect table:
/// | Rule | Worker/root operation | File scope | same harvest key | Effect |
/// |---|---|---|---|---|
/// | C1 | exact/live ReadyToDispose | None | first | publish unscoped File |
/// | C2 | exact/live | None | replay | same immutable File identity |
/// | C3 | exact later terminal effect | terminal op | same | associate terminal on same File |
/// | C4 | exact checkpoint | non-None | any | client rejects before HTTP/File |
/// | C5 | stale checkpoint operation | None | any | HTTP 409 before File |
/// Checkpoint release never consumes the one terminal association slot;
/// response loss is covered by the existing harvest key.
#[tokio::test]
async fn checkpoint_release_is_unscoped_idempotent_and_terminal_rebindable() {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let lifecycle = Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        files.clone(),
        files,
        lifecycle,
    ));
    let counting = Arc::new(CountingFileApplication::new(application.clone()));
    let (directory, identity) = support::ready_worker("worker-checkpoint-artifact").await;
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: identity.worker_id.clone(),
        runtime_incarnation: identity.lease_owner(),
        epoch: 9,
        expires_at_unix_ms: support::unix_now_ms().saturating_add(30_000),
    };
    let generation = awaken_session_contract::SandboxGeneration {
        id: "generation-a".into(),
        created_at_unix_ms: 0,
        expires_at_unix_ms: u64::MAX,
        environment_fingerprint: "environment-a".into(),
        base_image_fingerprint: "base-image-a".into(),
    };
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace-artifacts",
        "session-checkpoint-artifact",
        "suspend",
        &generation,
        3,
        Some(lease.clone()),
        None,
    );
    let terminal_effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        awaken_session_contract::SessionCleanupCommand::new(
            "session-checkpoint-artifact",
            "session-checkpoint-artifact",
            "cleanup-root",
        ),
        lease,
    );
    let service = Arc::new(
        WorkerArtifactPublicationService::new(
            counting.clone(),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
            directory,
        )
        .with_session_control(Arc::new(ExactArtifactControl {
            workspace_id: "workspace-artifacts".into(),
            terminal_effect: Some(terminal_effect.clone()),
            checkpoint: Some(("session-checkpoint-artifact".into(), operation.clone())),
        })),
    );
    let address = support::serve(worker_artifact_publication_router(service)).await;
    let publisher = HttpArtifactPublisher::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    );
    let checkpoint = checkpoint_release_publication(
        "workspace-artifacts",
        "session-checkpoint-artifact",
        "reports/checkpoint.txt",
        b"durable before source release",
        operation.clone(),
    );
    let first = publisher.publish(checkpoint.clone()).await.expect("C1");
    assert_eq!(first.record.artifact_idempotency_scope, None, "C1");
    assert_eq!(
        publisher.publish(checkpoint.clone()).await.expect("C2"),
        first,
        "C2"
    );

    let mut scoped_checkpoint = checkpoint.clone();
    scoped_checkpoint.idempotency_scope = Some("must-not-reserve-terminal".into());
    assert!(publisher.publish(scoped_checkpoint).await.is_err(), "C4");
    let mut stale_operation = operation;
    stale_operation.effect_id.push_str("-stale");
    assert!(
        publisher
            .publish(checkpoint_release_publication(
                "workspace-artifacts",
                "session-checkpoint-artifact",
                "reports/stale.txt",
                b"must not publish",
                stale_operation,
            ))
            .await
            .expect_err("C5")
            .to_string()
            .contains("409"),
        "C5"
    );

    let terminal = publisher
        .publish(terminal_publication(
            "workspace-artifacts",
            "session-checkpoint-artifact",
            "reports/checkpoint.txt",
            b"durable before source release",
            terminal_effect.clone(),
        ))
        .await
        .expect("C3");
    assert_eq!(terminal.record.id, first.record.id, "C3 same File");
    assert_eq!(
        terminal.record.artifact_idempotency_scope.as_deref(),
        Some(terminal_effect.operation_id()),
        "C3 terminal association"
    );
    assert_eq!(
        application
            .list("workspace-artifacts", Some("session-checkpoint-artifact"))
            .await
            .unwrap()
            .len(),
        1,
        "C1-C5 one immutable File"
    );
    assert_eq!(
        counting.artifact_creates.load(Ordering::SeqCst),
        3,
        "C4/C5 zero File effect"
    );
}

/// Terminal recovery cause/effect graph: C1 the asserted effect expired more
/// than 20s ago after its File mutation; C2 the current Worker is exact; C3 the
/// Control root either retains a live same-generation renewal, remains expired,
/// or has a foreign epoch; C4 durable evidence is current, unrelated, or
/// tombstoned. Effects:
/// E1 same-generation renewal recovers the exact receipt; E2 unrelated evidence
/// is filtered; E3 tombstones normalize to the original creation receipt; E4 a
/// current-expired root, foreign generation, or foreign Worker is rejected
/// before File reads or writes.
///
/// | Rule | Worker | asserted generation | current root | evidence | Effect |
/// |---|---|---|---|---|---|
/// | R1 | exact | expired >20s | same generation renewed/live | current | E1 |
/// | R2 | exact | expired >20s | same generation renewed/live | unrelated | E2 |
/// | R3 | exact | expired >20s | same generation renewed/live | tombstone | E3 |
/// | R4 | exact | expired >20s | same generation expired | any | E4/409, zero read/write |
/// | R5 | exact | successor epoch | predecessor current | any | E4/409 |
/// | R6 | foreign | any | any | any | E4/403 |
#[tokio::test]
async fn terminal_artifact_response_loss_recovers_only_exact_durable_file_evidence() {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let lifecycle = Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        files.clone(),
        files,
        lifecycle,
    ));
    let counting = Arc::new(CountingFileApplication::new(application.clone()));
    let (directory, identity) = support::ready_worker("worker-terminal-recovery").await;
    let now_unix_ms = support::unix_now_ms();
    let effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        awaken_session_contract::SessionCleanupCommand::new(
            "session-terminal-recovery",
            "session-terminal-recovery",
            "cleanup-root",
        ),
        awaken_session_contract::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 7,
            expires_at_unix_ms: now_unix_ms.saturating_sub(20_001),
        },
    );
    let renewed_effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        effect.command.clone(),
        awaken_session_contract::SessionRealizationLease {
            expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
            ..effect.lease.clone()
        },
    );
    let service = Arc::new(
        WorkerArtifactPublicationService::new(
            counting.clone(),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
            directory.clone(),
        )
        .with_session_control(Arc::new(ExactArtifactControl {
            workspace_id: "workspace-artifacts".into(),
            terminal_effect: Some(renewed_effect),
            checkpoint: None,
        })),
    );
    let address = support::serve(worker_artifact_publication_router(service)).await;
    let exact = HttpArtifactPublisher::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
    );

    let ordinary_bytes = b"ordinary evidence";
    let ordinary_publication = publication(
        None,
        "workspace-artifacts",
        "session-terminal-recovery",
        "ordinary.txt",
        ordinary_bytes,
    )
    .into_file_application();
    application
        .create_artifact(&ordinary_publication)
        .await
        .expect("R2 ordinary seed");
    let old_bytes = b"old terminal evidence";
    let mut old_publication = publication(
        None,
        "workspace-artifacts",
        "session-terminal-recovery",
        "old-terminal.txt",
        old_bytes,
    )
    .into_file_application();
    old_publication.idempotency_scope = Some("cleanup-old".into());
    application
        .create_artifact(&old_publication)
        .await
        .expect("R2 old terminal seed");

    let committed = exact
        .publish(terminal_publication(
            "workspace-artifacts",
            "session-terminal-recovery",
            "current.txt",
            b"committed before response loss",
            effect.clone(),
        ))
        .await
        .expect("R1 committed publication");
    let recovery = ArtifactRecovery {
        workspace_id: "workspace-artifacts".into(),
        session_id: "session-terminal-recovery".into(),
        idempotency_scope: effect.operation_id().into(),
        fence: ArtifactPublicationFence::Terminal(effect.clone()),
    };
    assert_eq!(
        exact.recover(recovery.clone()).await.expect("R1 recovery"),
        vec![committed.clone()],
        "R1/R2"
    );
    assert_eq!(counting.recovery_lists.load(Ordering::SeqCst), 1, "R1");
    assert_eq!(counting.artifact_creates.load(Ordering::SeqCst), 1, "R1");

    application
        .delete(
            "workspace-artifacts",
            &committed.record.id,
            support::unix_now_ms(),
        )
        .await
        .expect("R3 delete");
    assert_eq!(
        exact.recover(recovery.clone()).await.expect("R3 recovery"),
        vec![committed],
        "R3"
    );
    let reads_before_rejections = counting.recovery_lists.load(Ordering::SeqCst);
    let writes_before_rejections = counting.artifact_creates.load(Ordering::SeqCst);

    let expired_current_service = Arc::new(
        WorkerArtifactPublicationService::new(
            counting.clone(),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
            directory,
        )
        .with_session_control(Arc::new(ExactArtifactControl {
            workspace_id: "workspace-artifacts".into(),
            terminal_effect: Some(effect.clone()),
            checkpoint: None,
        })),
    );
    let expired_current_address =
        support::serve(worker_artifact_publication_router(expired_current_service)).await;
    let expired_current = HttpArtifactPublisher::new(
        WorkerUpstream::new(format!("http://{expired_current_address}"))
            .with_worker_identity(identity.clone()),
    );
    let expired_current_error = expired_current
        .recover(recovery.clone())
        .await
        .expect_err("R4 current root expiry closes response-loss recovery");
    assert!(
        expired_current_error.to_string().contains("409"),
        "R4 expired current: {expired_current_error}"
    );

    let mut successor = effect.clone();
    successor.lease.epoch += 1;
    successor.lease.expires_at_unix_ms = now_unix_ms.saturating_add(30_000);
    let successor_error = exact
        .recover(ArtifactRecovery {
            workspace_id: "workspace-artifacts".into(),
            session_id: "session-terminal-recovery".into(),
            idempotency_scope: successor.operation_id().into(),
            fence: ArtifactPublicationFence::Terminal(successor),
        })
        .await
        .expect_err("R5 successor epoch");
    assert!(
        successor_error.to_string().contains("409"),
        "R5 successor: {successor_error}"
    );

    let foreign =
        HttpArtifactPublisher::new(
            WorkerUpstream::new(format!("http://{address}")).with_worker_identity(
                WorkerIdentity::new("foreign-terminal-recovery", "foreign-boot", 1),
            ),
        );
    let foreign_error = foreign
        .recover(ArtifactRecovery {
            workspace_id: "workspace-artifacts".into(),
            session_id: "session-terminal-recovery".into(),
            idempotency_scope: effect.operation_id().into(),
            fence: ArtifactPublicationFence::Terminal(effect),
        })
        .await
        .expect_err("R6 foreign");
    assert!(foreign_error.to_string().contains("403"), "R6 foreign");
    assert_eq!(
        counting.recovery_lists.load(Ordering::SeqCst),
        reads_before_rejections,
        "R4-R6 zero File read"
    );
    assert_eq!(
        counting.artifact_creates.load(Ordering::SeqCst),
        writes_before_rejections,
        "R4-R6 zero File write"
    );
}
