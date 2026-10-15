// @formal evidence remote_worker.stale_epoch_never_mutates_truth
//! Session artifact publication over the real Worker HTTP boundary.

use awaken_dispatch::DispatchClaims;
use awaken_dispatch::DispatchCommitAuthority;
use awaken_dispatch::DispatchEnqueue;
use awaken_dispatch::DispatchExecution;
use awaken_dispatch::DispatchSettlement;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublisher as _, ArtifactRecovery,
    FileApplicationService, FileRecord, FileStore, FileStoreError, ResourcePurgeError, content_id,
};
use awaken_resource_worker_http::{
    ARTIFACT_METADATA_HEADER, ARTIFACT_PUBLICATION_PATH, ArtifactPublicationRequest,
    HttpArtifactPublisher, WorkerArtifactPublicationService, worker_artifact_publication_router,
};
use awaken_dispatch::{
    LeaseRenewalOutcome, MemoryDispatchStore, RunClaim, RunDispatch, WorkerIdentity,
};
use awaken_dispatch_testkit::worker_http as support;
use awaken_session_realization_contract::ResourceOperationFence;
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
        .claim(owner, 60_000, &Default::default())
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
) -> ArtifactPublication<ResourceOperationFence> {
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
        fence: claim.map(ResourceOperationFence::Run),
    }
}

fn terminal_publication(
    workspace: &str,
    session: &str,
    path: &str,
    bytes: &[u8],
    effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
) -> ArtifactPublication<ResourceOperationFence> {
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
        fence: Some(ResourceOperationFence::Terminal(effect)),
    }
}

fn checkpoint_release_publication(
    workspace: &str,
    session: &str,
    path: &str,
    bytes: &[u8],
    operation: awaken_session_state_contract::SessionEnvironmentOperation,
) -> ArtifactPublication<ResourceOperationFence> {
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
        fence: Some(ResourceOperationFence::CheckpointRelease(operation)),
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
    terminal_effect: Option<awaken_session_state_contract::SessionTerminalCleanupEffect>,
    checkpoint: Option<(
        String,
        awaken_session_state_contract::SessionEnvironmentOperation,
    )>,
}

struct CountingFileApplication {
    inner: Arc<dyn FileApplicationService>,
    recovery_lists: AtomicUsize,
    artifact_creates: AtomicUsize,
    capture_reads: AtomicUsize,
    capture_replaces: AtomicUsize,
}

impl CountingFileApplication {
    fn new(inner: Arc<dyn FileApplicationService>) -> Self {
        Self {
            inner,
            recovery_lists: AtomicUsize::new(0),
            artifact_creates: AtomicUsize::new(0),
            capture_reads: AtomicUsize::new(0),
            capture_replaces: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl FileApplicationService for CountingFileApplication {
    async fn artifact_capture(
        &self,
        workspace: &str,
        session: &str,
    ) -> Result<Option<awaken_resource_contract::ArtifactCapture>, ResourcePurgeError> {
        self.capture_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.artifact_capture(workspace, session).await
    }
    async fn replace_artifact_capture(
        &self,
        request: awaken_resource_contract::ReplaceArtifactCapture,
    ) -> Result<awaken_resource_contract::ReplaceArtifactCaptureOutcome, ResourcePurgeError> {
        self.capture_replaces.fetch_add(1, Ordering::SeqCst);
        self.inner.replace_artifact_capture(request).await
    }

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
impl awaken_session_realization_contract::SessionTerminalPreparationControl
    for ExactArtifactControl
{
    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        let Some(current) = self.terminal_effect.as_ref() else {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        };
        if current.command != effect.command
            || !awaken_session_realization_contract::realization_lease_generation_authorizes(
                &current.lease,
                &effect.lease,
            )
            || !awaken_session_realization_contract::realization_lease_is_live_at(
                current.lease.expires_at_unix_ms,
                support::unix_now_ms(),
            )
        {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            effect.clone(),
            self.workspace_id.clone(),
            None,
        )
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        _lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        _preparation: awaken_session_state_contract::SessionCleanupPreparation,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        Err(
            awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                "Artifact authorization fixture does not admit cleanup receipts".into(),
            ),
        )
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionCheckpointArtifactControl
    for ExactArtifactControl
{
    async fn authorize_checkpoint_release_artifact_effect(
        &self,
        session_id: &awaken_session_types::SessionId,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
    ) -> Result<String, awaken_session_realization_contract::SessionRealizationControlFailure> {
        if !self
            .checkpoint
            .as_ref()
            .is_some_and(|(expected_session, expected_operation)| {
                expected_session == session_id && expected_operation == operation
            })
        {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        Ok(self.workspace_id.clone())
    }
}

#[async_trait::async_trait]
impl FileStore for FailingFileStore {
    async fn put(&self, _bytes: &[u8]) -> Result<String, FileStoreError> {
        Err(FileStoreError::Indeterminate {
            detail: "injected storage outage".into(),
        })
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
/// | A9 | valid | yes | exact | traversal path | available | client preflight rejection; publish nothing |
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
    let lifecycle = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .unwrap(),
    );
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        files.clone(),
        files,
        lifecycle.clone(),
        awaken_time::system_clock(),
    ));
    let (directory, identity) = support::ready_worker("worker-artifacts").await;
    let dispatch = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    let claim = claimed_dispatch(&dispatch, "session-artifacts", &identity.lease_owner()).await;
    let service = Arc::new(WorkerArtifactPublicationService::new(
        application.clone(),
        dispatch.clone(),
        Arc::new(HeaderWorkerAuthenticator),
        directory.clone(),
        awaken_time::system_clock(),
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
            "bounded relative path",
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
        assert!(
            match status {
                "403" => matches!(error, ArtifactPublicationError::Denied { .. }),
                _ => matches!(error, ArtifactPublicationError::InvalidRequest { .. }),
            },
            "{rule}: {error}"
        );
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
    assert!(
        matches!(stale_identity, ArtifactPublicationError::Denied { .. }),
        "A8"
    );

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
        matches!(no_claim, ArtifactPublicationError::InvalidRequest { .. }),
        "A11"
    );

    dispatch
        .settle(
            &claim.run_id,
            claim.epoch,
            awaken_dispatch::DispatchOutcome::Done,
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
    assert!(
        matches!(stale_claim, ArtifactPublicationError::Conflict { .. }),
        "A5"
    );
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
        awaken_time::system_clock(),
    ));
    let unavailable = Arc::new(WorkerArtifactPublicationService::new(
        unavailable_application,
        dispatch.clone(),
        Arc::new(HeaderWorkerAuthenticator),
        directory,
        awaken_time::system_clock(),
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
    assert!(
        matches!(
            unavailable_error,
            ArtifactPublicationError::Indeterminate { .. }
        ),
        "A10"
    );
    assert!(
        dispatch.lock_commit_epoch(&unavailable_claim).await.is_ok(),
        "A10: a dependency outage must not settle or invalidate the retryable claim"
    );
}

#[tokio::test]
async fn embedded_and_raw_http_artifact_admission_have_the_same_resource_effects() {
    // Decision graph: publication metadata valid/invalid x embedded/raw HTTP.
    // P1 valid Unicode and exact path/MIME byte limits -> same immutable File;
    // P2 malformed metadata -> Invalid locally, HTTP 400 remotely, zero new
    // catalog/blob writes in either path. A raw request intentionally bypasses
    // HttpArtifactPublisher's preflight, proving the server also uses the rule.
    // P3 invalid + SDK publisher -> preflight rejection, not an assumed HTTP 400.
    // Existing A1-A12 separately own identity, epoch and outage combinations.
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        files.clone(),
        files.clone(),
        Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .await
                .unwrap(),
        ),
        awaken_time::system_clock(),
    ));
    let (directory, identity) = support::ready_worker("worker-admission").await;
    let dispatch = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    let claim = claimed_dispatch(&dispatch, "admission", &identity.lease_owner()).await;
    let address = support::serve(worker_artifact_publication_router(Arc::new(
        WorkerArtifactPublicationService::new(
            application.clone(),
            dispatch,
            Arc::new(HeaderWorkerAuthenticator),
            directory,
            awaken_time::system_clock(),
        ),
    )))
    .await;
    let upstream =
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone());
    let publisher = HttpArtifactPublisher::new(upstream.clone());

    for fault in [
        "valid",
        "path_limit",
        "mime_limit",
        "empty_path",
        "absolute",
        "dot",
        "parent",
        "empty_segment",
        "trailing_slash",
        "long_path",
        "empty_mime",
        "long_mime",
        "empty_workspace",
        "empty_session",
        "empty_scope",
    ] {
        let mut candidate = publication(
            Some(claim.clone()),
            "workspace-artifacts",
            "thread-admission",
            "报告/销售 数据.xlsx",
            fault.as_bytes(),
        );
        match fault {
            "path_limit" => candidate.logical_path = "x".repeat(1024),
            "mime_limit" => candidate.mime_type = "x".repeat(255),
            "empty_path" => candidate.logical_path.clear(),
            "absolute" => candidate.logical_path = "/result".into(),
            "dot" => candidate.logical_path = "a/./b".into(),
            "parent" => candidate.logical_path = "a/../b".into(),
            "empty_segment" => candidate.logical_path = "a//b".into(),
            "trailing_slash" => candidate.logical_path = "a/".into(),
            "long_path" => candidate.logical_path = "x".repeat(1025),
            "empty_mime" => candidate.mime_type = "  ".into(),
            "long_mime" => candidate.mime_type = "x".repeat(256),
            "empty_workspace" => candidate.workspace_id = " ".into(),
            "empty_session" => candidate.session_id = " ".into(),
            "empty_scope" => candidate.idempotency_scope = Some(" ".into()),
            "valid" => {}
            _ => unreachable!(),
        }
        candidate.effect_id = awaken_resource_contract::harvest_idempotency_key(
            &candidate.session_id,
            &candidate.logical_path,
            &candidate.content_id,
        );
        let valid = matches!(fault, "valid" | "path_limit" | "mime_limit");
        let before_files = application
            .list(&candidate.workspace_id, None)
            .await
            .unwrap();
        let before_blobs = files.list().await.unwrap();
        let local = application
            .create_artifact(&candidate.clone().into_file_application())
            .await;
        assert_eq!(local.is_ok(), valid, "P1/P2 embedded {fault}");
        if !valid {
            assert!(
                matches!(local, Err(ResourcePurgeError::InvalidIntent { .. })),
                "P2 {fault}"
            );
            assert!(
                publisher.publish(candidate.clone()).await.is_err(),
                "P3 {fault}"
            );
        }
        let metadata = ArtifactPublicationRequest {
            operation_fence: candidate.fence.clone().unwrap(),
            identity: Some(identity.clone()),
            workspace_id: candidate.workspace_id.clone(),
            session_id: candidate.session_id.clone(),
            effect_id: candidate.effect_id.clone(),
            logical_path: candidate.logical_path.clone(),
            mime_type: candidate.mime_type.clone(),
            content_id: candidate.content_id.clone(),
            idempotency_scope: candidate.idempotency_scope.clone(),
        };
        let request = upstream
            .http_client()
            .post(format!("http://{address}{ARTIFACT_PUBLICATION_PATH}"))
            .header(
                ARTIFACT_METADATA_HEADER,
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(serde_json::to_vec(&metadata).unwrap()),
            )
            .body(candidate.bytes);
        let response = upstream
            .authorize_request("POST", ARTIFACT_PUBLICATION_PATH, request)
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if valid {
                reqwest::StatusCode::OK
            } else {
                reqwest::StatusCode::BAD_REQUEST
            },
            "P1/P2 raw HTTP {fault}"
        );
        if valid {
            let receipt: awaken_resource_contract::ArtifactPublicationReceipt =
                response.json().await.unwrap();
            assert_eq!(receipt.record.id, local.unwrap().id, "P1 same File {fault}");
        } else {
            assert_eq!(
                application
                    .list(&candidate.workspace_id, None)
                    .await
                    .unwrap(),
                before_files,
                "P2 zero File writes {fault}"
            );
            assert_eq!(
                files.list().await.unwrap(),
                before_blobs,
                "P2 zero blob writes {fault}"
            );
        }
    }
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
    let lifecycle = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .unwrap(),
    );
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        files.clone(),
        files,
        lifecycle,
        awaken_time::system_clock(),
    ));
    let (directory, identity) = support::ready_worker("worker-terminal-artifact").await;
    let now_unix_ms = support::unix_now_ms();
    let asserted = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        awaken_session_state_contract::SessionCleanupCommand::new(
            &awaken_session_types::SessionId::from("session-terminal-artifact"),
            "session-terminal-artifact",
            "cleanup-root",
        ),
        awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 4,
            expires_at_unix_ms: now_unix_ms.saturating_sub(20_001),
        },
    );
    let renewed = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        asserted.command.clone(),
        awaken_session_configuration_contract::resource::SessionRealizationLease {
            expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
            ..asserted.lease.clone()
        },
    );
    let service = Arc::new(
        WorkerArtifactPublicationService::new(
            application.clone(),
            Arc::new(MemoryDispatchStore::new(awaken_time::system_clock())),
            Arc::new(HeaderWorkerAuthenticator),
            directory.clone(),
            awaken_time::system_clock(),
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

    let receipt = exact
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
            Arc::new(MemoryDispatchStore::new(awaken_time::system_clock())),
            Arc::new(HeaderWorkerAuthenticator),
            directory,
            awaken_time::system_clock(),
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
    assert!(
        matches!(error, ArtifactPublicationError::Conflict { .. }),
        "T2: {error}"
    );

    let mut successor = asserted.clone();
    successor.lease.epoch += 1;
    successor.lease.expires_at_unix_ms = now_unix_ms.saturating_add(30_000);
    let error = exact
        .publish(terminal_publication(
            "workspace-artifacts",
            "session-terminal-artifact",
            "terminal/successor.txt",
            b"must-not-publish",
            successor.clone(),
        ))
        .await
        .expect_err("T3 a foreign epoch cannot use the renewed predecessor");
    assert!(
        matches!(error, ArtifactPublicationError::Conflict { .. }),
        "T3: {error}"
    );

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
            asserted.clone(),
        ))
        .await
        .expect_err("T4 foreign terminal Worker");
    assert!(
        matches!(error, ArtifactPublicationError::Denied { .. }),
        "T4: {error}"
    );

    // T5 capture read/replacement use the same current-root decision as
    // publication: renewed exact assertion passes; expired current root,
    // foreign generation and foreign Worker reject without changing the head.
    let capture_fence = ResourceOperationFence::Terminal(asserted.clone());
    let capture = awaken_resource_contract::ReplaceArtifactCapture {
        origin: None,
        workspace_id: "workspace-artifacts".into(),
        session_id: "session-terminal-artifact".into(),
        expected_revision: 0,
        file_ids: vec![receipt.record.id.clone()],
    };
    assert_eq!(
        exact
            .capture(
                &capture.workspace_id,
                &capture.session_id,
                Some(&capture_fence)
            )
            .await
            .unwrap(),
        None
    );
    assert!(
        matches!(
            exact
                .replace_capture(capture.clone(), Some(&capture_fence))
                .await
                .unwrap(),
            awaken_resource_contract::ReplaceArtifactCaptureOutcome::Applied(_)
        ),
        "T5"
    );
    let successor_fence = ResourceOperationFence::Terminal(successor);
    for (peer, fence) in [
        (&unrenewed, &capture_fence),
        (&exact, &successor_fence),
        (&foreign, &capture_fence),
    ] {
        assert!(
            peer.capture(&capture.workspace_id, &capture.session_id, Some(fence))
                .await
                .is_err(),
            "T5 read denial"
        );
        assert!(
            peer.replace_capture(capture.clone(), Some(fence))
                .await
                .is_err(),
            "T5 replay still fenced"
        );
    }
    assert_eq!(
        application
            .artifact_capture(&capture.workspace_id, &capture.session_id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        1
    );

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
    let lifecycle = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .unwrap(),
    );
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        files.clone(),
        files,
        lifecycle,
        awaken_time::system_clock(),
    ));
    let counting = Arc::new(CountingFileApplication::new(application.clone()));
    let (directory, identity) = support::ready_worker("worker-checkpoint-artifact").await;
    let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
        owner: identity.worker_id.clone(),
        runtime_incarnation: identity.lease_owner(),
        epoch: 9,
        expires_at_unix_ms: support::unix_now_ms().saturating_add(30_000),
    };
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from("session-checkpoint-artifact"),
        0,
        u64::MAX,
        "environment-a",
        "base-image-a",
    );
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace-artifacts",
        &awaken_session_types::SessionId::from("session-checkpoint-artifact"),
        "suspend",
        &generation,
        3,
        Some(lease.clone()),
        None,
    );
    let terminal_effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        awaken_session_state_contract::SessionCleanupCommand::new(
            &awaken_session_types::SessionId::from("session-checkpoint-artifact"),
            "session-checkpoint-artifact",
            "cleanup-root",
        ),
        lease,
    );
    let service = Arc::new(
        WorkerArtifactPublicationService::new(
            counting.clone(),
            Arc::new(MemoryDispatchStore::new(awaken_time::system_clock())),
            Arc::new(HeaderWorkerAuthenticator),
            directory,
            awaken_time::system_clock(),
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

    // C6 the same checkpoint root also owns capture reads/replacements.
    // A stale operation must be rejected before any capture catalog I/O.
    let capture_fence = checkpoint.fence.as_ref().unwrap();
    let capture = awaken_resource_contract::ReplaceArtifactCapture {
        origin: None,
        workspace_id: checkpoint.workspace_id.clone(),
        session_id: checkpoint.session_id.clone(),
        expected_revision: 0,
        file_ids: vec![first.record.id.clone()],
    };
    assert_eq!(
        publisher
            .capture(
                &capture.workspace_id,
                &capture.session_id,
                Some(capture_fence)
            )
            .await
            .unwrap(),
        None
    );
    assert!(
        matches!(
            publisher
                .replace_capture(capture.clone(), Some(capture_fence))
                .await
                .unwrap(),
            awaken_resource_contract::ReplaceArtifactCaptureOutcome::Applied(_)
        ),
        "C6"
    );

    let mut scoped_checkpoint = checkpoint.clone();
    scoped_checkpoint.idempotency_scope = Some("must-not-reserve-terminal".into());
    assert!(publisher.publish(scoped_checkpoint).await.is_err(), "C4");
    let mut stale_operation = operation;
    stale_operation.effect_id.push_str("-stale");
    assert!(
        matches!(
            publisher
                .publish(checkpoint_release_publication(
                    "workspace-artifacts",
                    "session-checkpoint-artifact",
                    "reports/stale.txt",
                    b"must not publish",
                    stale_operation.clone(),
                ))
                .await
                .expect_err("C5"),
            ArtifactPublicationError::Conflict { .. }
        ),
        "C5"
    );

    let stale_fence = ResourceOperationFence::CheckpointRelease(stale_operation);
    assert!(
        publisher
            .capture(
                &capture.workspace_id,
                &capture.session_id,
                Some(&stale_fence)
            )
            .await
            .is_err(),
        "C6 stale read"
    );
    assert!(
        publisher
            .replace_capture(capture.clone(), Some(&stale_fence))
            .await
            .is_err(),
        "C6 stale replay"
    );
    assert_eq!(
        counting.capture_reads.load(Ordering::SeqCst),
        1,
        "C6 zero stale reads"
    );
    assert_eq!(
        counting.capture_replaces.load(Ordering::SeqCst),
        1,
        "C6 zero stale writes"
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
    let lifecycle = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .unwrap(),
    );
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        files.clone(),
        files,
        lifecycle,
        awaken_time::system_clock(),
    ));
    let counting = Arc::new(CountingFileApplication::new(application.clone()));
    let (directory, identity) = support::ready_worker("worker-terminal-recovery").await;
    let now_unix_ms = support::unix_now_ms();
    let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        awaken_session_state_contract::SessionCleanupCommand::new(
            &awaken_session_types::SessionId::from("session-terminal-recovery"),
            "session-terminal-recovery",
            "cleanup-root",
        ),
        awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 7,
            expires_at_unix_ms: now_unix_ms.saturating_sub(20_001),
        },
    );
    let renewed_effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        effect.command.clone(),
        awaken_session_configuration_contract::resource::SessionRealizationLease {
            expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
            ..effect.lease.clone()
        },
    );
    let service = Arc::new(
        WorkerArtifactPublicationService::new(
            counting.clone(),
            Arc::new(MemoryDispatchStore::new(awaken_time::system_clock())),
            Arc::new(HeaderWorkerAuthenticator),
            directory.clone(),
            awaken_time::system_clock(),
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
        fence: ResourceOperationFence::Terminal(effect.clone()),
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
            Arc::new(MemoryDispatchStore::new(awaken_time::system_clock())),
            Arc::new(HeaderWorkerAuthenticator),
            directory,
            awaken_time::system_clock(),
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
        matches!(
            expired_current_error,
            ArtifactPublicationError::Conflict { .. }
        ),
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
            fence: ResourceOperationFence::Terminal(successor),
        })
        .await
        .expect_err("R5 successor epoch");
    assert!(
        matches!(successor_error, ArtifactPublicationError::Conflict { .. }),
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
            fence: ResourceOperationFence::Terminal(effect),
        })
        .await
        .expect_err("R6 foreign");
    assert!(
        matches!(foreign_error, ArtifactPublicationError::Denied { .. }),
        "R6 foreign"
    );
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

#[tokio::test]
async fn complete_capture_round_trip_is_fenced_and_never_rebases_stale_outputs() {
    // C1 exact Worker identity; C2 live exact Run fence; C3 exact scope;
    // C4 expected revision; C5 all File references valid; C6 response replay.
    // R1 C1..C5 -> Applied; R2 C1..C3+C6 -> Replayed; R3 !C4 -> Conflict;
    // R4 !C1|!C2|!C3 -> zero capture reads/replacements; R5 !C5 -> old set
    // unchanged. R6 A->B->A selects the reused A id, never newest File metadata.
    use awaken_resource_contract::{
        ReplaceArtifactCapture, ReplaceArtifactCaptureOutcome as Outcome,
    };
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let app = Arc::new(awaken_resource_application::FileApplication::new(
        files.clone(),
        files,
        Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .await
                .unwrap(),
        ),
        awaken_time::system_clock(),
    ));
    let counting = Arc::new(CountingFileApplication::new(app.clone()));
    let (directory, identity) = support::ready_worker("capture-worker").await;
    // R10 needs a lease that is already expired. The store now judges that
    // from its own clock, so this fixture owns one it can move.
    let dispatch_clock = Arc::new(awaken_lease_testkit::ManualClock::new(
        support::unix_now_ms(),
    ));
    let dispatch = Arc::new(MemoryDispatchStore::new(dispatch_clock.clone()));
    let claim = claimed_dispatch(&dispatch, "capture-journey", &identity.lease_owner()).await;
    let service = Arc::new(WorkerArtifactPublicationService::new(
        counting.clone(),
        dispatch.clone(),
        Arc::new(HeaderWorkerAuthenticator),
        directory,
        awaken_time::system_clock(),
    ));
    // R9 the actual handler commits, then a middleware loses its successful
    // response. Retrying the exact request must return Replayed, not revision+1.
    async fn lose_capture_ack(
        axum::extract::State(armed): axum::extract::State<Arc<std::sync::atomic::AtomicBool>>,
        request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        use axum::response::IntoResponse as _;
        let lose = request.uri().path() == awaken_resource_worker_http::ARTIFACT_CAPTURE_PATH
            && armed.swap(false, Ordering::SeqCst);
        let response = next.run(request).await;
        if lose && response.status().is_success() {
            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
        } else {
            response
        }
    }
    let lose_ack = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let router = worker_artifact_publication_router(service).layer(
        axum::middleware::from_fn_with_state(lose_ack.clone(), lose_capture_ack),
    );
    let address = support::serve(router).await;
    let upstream =
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone());
    let publisher = HttpArtifactPublisher::new(upstream.clone());
    let fence = ResourceOperationFence::Run(claim.clone());
    let workspace = "workspace-artifacts";
    let session = "thread-capture-journey";
    assert_eq!(
        publisher
            .capture(workspace, session, Some(&fence))
            .await
            .unwrap(),
        None,
        "R1 unknown"
    );
    let mut published = Vec::new();
    for (revision, bytes) in [b"A", b"B", b"A"].into_iter().enumerate() {
        let receipt = publisher
            .publish(publication(
                Some(claim.clone()),
                workspace,
                session,
                "report.txt",
                bytes,
            ))
            .await
            .unwrap();
        let request = ReplaceArtifactCapture {
            origin: None,
            workspace_id: workspace.into(),
            session_id: session.into(),
            expected_revision: revision as u64,
            file_ids: vec![receipt.record.id.clone()],
        };
        assert!(
            matches!(
                publisher
                    .replace_capture(request.clone(), Some(&fence))
                    .await
                    .unwrap(),
                Outcome::Applied(_)
            ),
            "R1/R6"
        );
        assert!(
            matches!(
                publisher
                    .replace_capture(request, Some(&fence))
                    .await
                    .unwrap(),
                Outcome::Replayed(_)
            ),
            "R2"
        );
        published.push(receipt.record.id);
    }
    assert_eq!(published[0], published[2], "R6");
    assert_ne!(published[0], published[1], "R6");
    let stable = publisher
        .capture(workspace, session, Some(&fence))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stable.file_ids, [published[0].clone()]);
    // R11 capture provenance comes from the authorized original claim, not
    // a caller label. Both the client and a raw authenticated Worker request
    // reject foreign Run/owner/epoch before any catalog replacement.
    let original = stable.origin.as_ref().expect("R11 original Run reference");
    assert_eq!(
        (&*original.run_id, &*original.owner, original.epoch.get()),
        (&*claim.run_id.0, &*claim.owner, claim.epoch),
        "R11 exact original claim"
    );
    let request = ReplaceArtifactCapture {
        origin: None,
        workspace_id: workspace.into(),
        session_id: session.into(),
        expected_revision: 3,
        file_ids: vec![published[0].clone()],
    };
    let mut stale = request.clone();
    stale.expected_revision = 0;
    assert_eq!(
        publisher
            .replace_capture(stale, Some(&fence))
            .await
            .unwrap(),
        Outcome::Conflict,
        "R3"
    );
    let mut missing = request.clone();
    missing.file_ids = vec!["missing-file".into()];
    assert!(
        publisher
            .replace_capture(missing, Some(&fence))
            .await
            .is_err(),
        "R5"
    );
    assert_eq!(
        publisher
            .capture(workspace, session, Some(&fence))
            .await
            .unwrap(),
        Some(stable.clone())
    );
    let reads = counting.capture_reads.load(Ordering::SeqCst);
    let replacements = counting.capture_replaces.load(Ordering::SeqCst);
    for field in 0..3 {
        let mut forged = request.clone();
        forged.origin = stable.origin.clone();
        let origin = forged.origin.as_mut().unwrap();
        match field {
            0 => origin.run_id = "foreign-run".into(),
            1 => origin.owner = "foreign-owner".into(),
            _ => origin.epoch = std::num::NonZeroU64::new(u64::MAX).unwrap(),
        }
        assert!(
            publisher
                .replace_capture(forged.clone(), Some(&fence))
                .await
                .is_err(),
            "R11 client {field}"
        );
        let payload = awaken_resource_worker_http::ArtifactCaptureRequest {
            operation_fence: fence.clone(),
            identity: Some(identity.clone()),
            operation: awaken_resource_worker_http::ArtifactCaptureOperation::Replace {
                request: forged,
            },
        };
        let url = format!(
            "http://{address}{}",
            awaken_resource_worker_http::ARTIFACT_CAPTURE_PATH
        );
        let response = upstream
            .authorize_request(
                "POST",
                awaken_resource_worker_http::ARTIFACT_CAPTURE_PATH,
                upstream.http_client().post(&url).json(&payload),
            )
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::FORBIDDEN,
            "R11 server {field}"
        );
        assert_eq!(
            counting.capture_replaces.load(Ordering::SeqCst),
            replacements,
            "R11 zero replacements"
        );
    }
    let mut stale_claim = claim.clone();
    stale_claim.epoch += 1;
    let stale_fence = ResourceOperationFence::Run(stale_claim);
    for (case, ws, sid, operation_fence) in [
        ("workspace", "foreign", session, Some(&fence)),
        ("session", workspace, "foreign", Some(&fence)),
        ("epoch", workspace, session, Some(&stale_fence)),
        ("missing fence", workspace, session, None),
    ] {
        assert!(
            publisher.capture(ws, sid, operation_fence).await.is_err(),
            "R4 read {case}"
        );
        let mut denied = request.clone();
        denied.workspace_id = ws.into();
        denied.session_id = sid.into();
        assert!(
            publisher
                .replace_capture(denied, operation_fence)
                .await
                .is_err(),
            "R4 replace {case}"
        );
    }
    let wrong_worker =
        HttpArtifactPublisher::new(
            WorkerUpstream::new(format!("http://{address}")).with_worker_identity(
                WorkerIdentity::new("capture-worker", "other-incarnation", 2),
            ),
        );
    assert!(
        wrong_worker
            .capture(workspace, session, Some(&fence))
            .await
            .is_err(),
        "R4 Worker read"
    );
    assert!(
        wrong_worker
            .replace_capture(request.clone(), Some(&fence))
            .await
            .is_err(),
        "R4 Worker replace"
    );
    assert_eq!(
        counting.capture_reads.load(Ordering::SeqCst),
        reads,
        "R4 zero reads"
    );
    assert_eq!(
        counting.capture_replaces.load(Ordering::SeqCst),
        replacements,
        "R4 zero replacements"
    );

    // R7 no Worker authentication must fail before even an exact request can
    // observe the capture. R8 raw invalid ids cannot bypass client validation.
    use awaken_resource_worker_http::{
        ARTIFACT_CAPTURE_PATH, ArtifactCaptureOperation, ArtifactCaptureRequest,
    };
    let payload = ArtifactCaptureRequest {
        operation_fence: fence.clone(),
        identity: Some(identity.clone()),
        operation: ArtifactCaptureOperation::Read {
            workspace_id: workspace.into(),
            session_id: session.into(),
        },
    };
    let url = format!("http://{address}{ARTIFACT_CAPTURE_PATH}");
    assert_eq!(
        reqwest::Client::new()
            .post(&url)
            .json(&payload)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "R7"
    );
    let mut invalid = request;
    invalid.file_ids = vec![published[0].clone(), published[0].clone()];
    let payload = ArtifactCaptureRequest {
        operation: ArtifactCaptureOperation::Replace { request: invalid },
        ..payload
    };
    let http = upstream.http_client().post(&url).json(&payload);
    let response = upstream
        .authorize_request("POST", ARTIFACT_CAPTURE_PATH, http)
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST, "R8");
    assert_eq!(
        counting.capture_reads.load(Ordering::SeqCst),
        reads,
        "R7/R8"
    );
    assert_eq!(
        counting.capture_replaces.load(Ordering::SeqCst),
        replacements,
        "R7/R8"
    );
    assert_eq!(
        app.artifact_capture(workspace, session).await.unwrap(),
        Some(stable)
    );
    let lost = awaken_resource_contract::ReplaceArtifactCapture {
        origin: None,
        workspace_id: workspace.into(),
        session_id: session.into(),
        expected_revision: 3,
        file_ids: vec![published[0].clone()],
    };
    lose_ack.store(true, Ordering::SeqCst);
    assert!(
        matches!(
            publisher
                .replace_capture(lost.clone(), Some(&fence))
                .await
                .unwrap_err(),
            ArtifactPublicationError::Indeterminate { .. }
        ),
        "R9 missing ACK"
    );
    let committed = app
        .artifact_capture(workspace, session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.revision, 4, "R9 mutation already committed");
    assert_eq!(
        publisher.replace_capture(lost, Some(&fence)).await.unwrap(),
        Outcome::Replayed(committed.clone()),
        "R9 exact receipt"
    );
    assert_eq!(
        publisher
            .capture(workspace, session, Some(&fence))
            .await
            .unwrap(),
        Some(committed),
        "R9 no extra revision"
    );

    // R11 a child shares parent recovery/history affinity, but File effects
    // belong to its actual execution Thread. Exact child publication/read/CAS
    // succeed; changing only the target to its parent yields 403 before File
    // I/O. The parent's committed capture and immutable Files remain unchanged.
    let child_request = RunDispatch::new(support::activation("capture-child"))
        .for_session(awaken_agent_contract::agent::thread::Id(session.into()))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from(workspace),
        ));
    let child_session = child_request.thread_id().0.clone();
    dispatch.enqueue(child_request).await.unwrap();
    let child_lease = dispatch
        .claim(&identity.lease_owner(), 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        child_lease.request.thread_id().0,
        child_session,
        "R11 child claimed"
    );
    let child_claim = RunClaim::from(&child_lease.lease);
    let child_fence = ResourceOperationFence::Run(child_claim.clone());
    assert_eq!(
        publisher
            .capture(workspace, &child_session, Some(&child_fence))
            .await
            .unwrap(),
        None,
        "R11 unknown child"
    );
    let child_file = publisher
        .publish(publication(
            Some(child_claim.clone()),
            workspace,
            &child_session,
            "report.txt",
            b"child",
        ))
        .await
        .unwrap();
    let child_capture = ReplaceArtifactCapture {
        origin: None,
        workspace_id: workspace.into(),
        session_id: child_session.clone(),
        expected_revision: 0,
        file_ids: vec![child_file.record.id.clone()],
    };
    assert!(
        matches!(
            publisher
                .replace_capture(child_capture, Some(&child_fence))
                .await
                .unwrap(),
            Outcome::Applied(_)
        ),
        "R11 child capture"
    );
    let observed_child = publisher
        .capture(workspace, &child_session, Some(&child_fence))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed_child.revision, 1, "R11");
    assert_eq!(observed_child.file_ids, vec![child_file.record.id], "R11");
    let reads = counting.capture_reads.load(Ordering::SeqCst);
    let replacements = counting.capture_replaces.load(Ordering::SeqCst);
    let parent_files = app.list(workspace, Some(session)).await.unwrap();
    assert!(
        matches!(
            publisher
                .publish(publication(
                    Some(child_claim),
                    workspace,
                    session,
                    "report.txt",
                    b"forbidden"
                ))
                .await
                .unwrap_err(),
            ArtifactPublicationError::Denied { .. }
        ),
        "R11 parent publication"
    );
    assert!(
        matches!(
            publisher
                .capture(workspace, session, Some(&child_fence))
                .await
                .unwrap_err(),
            ArtifactPublicationError::Denied { .. }
        ),
        "R11 parent read"
    );
    assert!(
        matches!(
            publisher
                .replace_capture(
                    ReplaceArtifactCapture {
                        origin: None,
                        workspace_id: workspace.into(),
                        session_id: session.into(),
                        expected_revision: 4,
                        file_ids: vec![],
                    },
                    Some(&child_fence)
                )
                .await
                .unwrap_err(),
            ArtifactPublicationError::Denied { .. }
        ),
        "R11 parent replacement"
    );
    assert_eq!(
        counting.capture_reads.load(Ordering::SeqCst),
        reads,
        "R11 zero parent reads"
    );
    assert_eq!(
        counting.capture_replaces.load(Ordering::SeqCst),
        replacements,
        "R11 zero parent writes"
    );
    assert_eq!(
        app.list(workspace, Some(session)).await.unwrap(),
        parent_files,
        "R11 no parent File mutation"
    );
    let parent_capture = app
        .artifact_capture(workspace, session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parent_capture.revision, 4, "R11 parent revision unchanged");
    assert_eq!(
        parent_capture.file_ids,
        vec![published[0].clone()],
        "R11 parent output unchanged"
    );

    // R10 a retained exact epoch with an expired lease first returns the typed
    // Current renewal outcome, but still authorizes no capture I/O. This exact
    // match preserves the distinction from CancellationRequested and Lost.
    // Advance the isolated fixture to that state without sleeping.
    let reads = counting.capture_reads.load(Ordering::SeqCst);
    let replacements = counting.capture_replaces.load(Ordering::SeqCst);
    assert_eq!(
        dispatch.renew_lease(&claim, 0).await.unwrap(),
        LeaseRenewalOutcome::Current,
        "R10 exact current claim receives the typed renewal outcome"
    );
    // The zero-duration renewal expires at the reading it was minted from.
    dispatch_clock.set(support::unix_now_ms().saturating_add(1));
    assert!(
        matches!(
            publisher
                .capture(workspace, session, Some(&fence))
                .await
                .unwrap_err(),
            ArtifactPublicationError::Conflict { .. }
        ),
        "R10"
    );
    assert!(
        matches!(
            publisher
                .replace_capture(
                    awaken_resource_contract::ReplaceArtifactCapture {
                        origin: None,
                        workspace_id: workspace.into(),
                        session_id: session.into(),
                        expected_revision: 4,
                        file_ids: vec![published[0].clone()],
                    },
                    Some(&fence)
                )
                .await
                .unwrap_err(),
            ArtifactPublicationError::Conflict { .. }
        ),
        "R10"
    );
    assert_eq!(
        counting.capture_reads.load(Ordering::SeqCst),
        reads,
        "R10 zero reads"
    );
    assert_eq!(
        counting.capture_replaces.load(Ordering::SeqCst),
        replacements,
        "R10 zero writes"
    );
}

#[tokio::test]
async fn capture_client_rejects_wrong_operation_scope_revision_and_file_set() {
    // P1 a 200 reply is not authority by itself. Read replies must be canonical
    // captures in the requested scope; replacement receipts must acknowledge
    // the exact requested successor and complete id set. P2 a different response
    // operation cannot be decoded as success. These are peer-response failures,
    // not evidence that a remote write was rolled back or is safe to repeat.
    use awaken_resource_worker_http::ARTIFACT_CAPTURE_PATH;
    let claim = RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("run".into()),
        owner: "owner".into(),
        epoch: 1,
    };
    let fence = ResourceOperationFence::Run(claim);
    let canonical = serde_json::json!({"workspace_id":"workspace", "session_id":"session", "revision":1, "file_ids":["a"]});
    for operation in ["read", "replace"] {
        for field in [
            "workspace_id",
            "session_id",
            "revision",
            "file_ids",
            "operation",
        ] {
            let mut capture = canonical.clone();
            match field {
                "workspace_id" | "session_id" => capture[field] = serde_json::json!("foreign"),
                "revision" => capture[field] = serde_json::json!(0),
                "file_ids" => {
                    capture[field] = if operation == "read" {
                        serde_json::json!(["b", "a"])
                    } else {
                        serde_json::json!(["b"])
                    }
                }
                _ => (),
            }
            let reply = if operation == "read" && field != "operation"
                || operation == "replace" && field == "operation"
            {
                serde_json::json!({"result":"current", "value":capture})
            } else {
                serde_json::json!({"result":"replaced", "value":{"outcome":"applied", "capture":capture}})
            };
            let router = axum::Router::new().route(
                ARTIFACT_CAPTURE_PATH,
                axum::routing::post(move || {
                    let reply = reply.clone();
                    async move { axum::Json(reply) }
                }),
            );
            let address = support::serve(router).await;
            let peer = HttpArtifactPublisher::new(WorkerUpstream::new(format!("http://{address}")));
            if operation == "read" {
                assert!(
                    peer.capture("workspace", "session", Some(&fence))
                        .await
                        .is_err(),
                    "P1/P2 {operation}/{field}"
                );
            } else {
                assert!(
                    peer.replace_capture(
                        awaken_resource_contract::ReplaceArtifactCapture {
                            origin: None,
                            workspace_id: "workspace".into(),
                            session_id: "session".into(),
                            expected_revision: 0,
                            file_ids: vec!["a".into()],
                        },
                        Some(&fence)
                    )
                    .await
                    .is_err(),
                    "P1/P2 {operation}/{field}"
                );
            }
        }
    }
}
