//! Session artifact publication over the real Worker HTTP boundary.

use std::sync::Arc;

use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublisher as _, FileStore, FileStoreError, content_id,
};
use awaken_resource_worker_http::{
    ARTIFACT_METADATA_HEADER, ARTIFACT_PUBLICATION_PATH, HttpArtifactPublisher,
    WorkerArtifactPublicationService, worker_artifact_publication_router,
};
use awaken_run_ingress::{
    DispatchQueue as _, MemoryDispatchStore, RunClaim, RunDispatch, WorkerIdentity,
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
) -> ArtifactPublication<RunClaim> {
    ArtifactPublication {
        workspace_id: workspace.into(),
        session_id: session.into(),
        logical_path: path.into(),
        mime_type: "text/html".into(),
        bytes: bytes.to_vec(),
        fence: claim,
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
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            "claim": claim,
            "identity": identity,
            "workspace_id": workspace,
            "session_id": session,
            "logical_path": path,
            "mime_type": "text/html",
            "content_id": digest,
        }))
        .unwrap(),
    )
}

struct FailingFileStore;

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
/// | A9 | valid | yes | exact | traversal path | available | 403; publish nothing |
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
        first.scope_id.as_deref(),
        Some("thread-session-artifacts"),
        "A1"
    );
    assert_eq!(
        first.logical_path.as_deref(),
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
    assert_eq!(retry.id, first.id, "A2");

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
    assert_ne!(changed.id, first.id, "A3");
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

    for (workspace, session, path, rule) in [
        (
            "workspace-foreign",
            "thread-session-artifacts",
            "scope.html",
            "A6",
        ),
        (
            "workspace-artifacts",
            "session-foreign",
            "session.html",
            "A6",
        ),
        (
            "workspace-artifacts",
            "thread-session-artifacts",
            "../escape.html",
            "A9",
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
        assert!(error.to_string().contains("403"), "{rule}: {error}");
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
        no_claim.to_string().contains("requires a dispatch claim"),
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
