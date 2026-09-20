//! Output selection over the product Resources router and the original Files API.
use awaken_protocol_managed::{ResourcesRouterInput, resources_router};
use awaken_resource_contract::{
    ArtifactCapture, ArtifactPublication, CreateFileRecordOutcome, FileApplicationService,
    FileCatalog, FileCatalogError, FileRecord, ReplaceArtifactCapture,
};
use awaken_tenancy::WorkspaceScope;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;
mod support;

fn mounted(files: Arc<dyn FileApplicationService>) -> Router {
    let resources = support::resources::ephemeral_resources();
    let authorities = resources.authorities();
    resources_router(ResourcesRouterInput {
        repositories: resources.repositories(Arc::new(
            awaken_resource_contract::UnavailableRepositoryCredentialValidator,
        )),
        files,
        memories: authorities.memory_repository(),
        memory_stores: resources.memory_stores(),
        skills: Some(authorities.skill_store()),
        purge: resources.purge_scheduler(),
    })
}
async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    scope: Option<&str>,
) -> support::Call<Vec<u8>> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("anthropic-beta", "files-api-2025-04-14")
        .body(Body::empty())
        .unwrap();
    if let Some(scope) = scope {
        request
            .extensions_mut()
            .insert(WorkspaceScope(scope.into()));
    }
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    support::Call::new(status, bytes)
}
const URI: &str = "/v1/awaken/files/output-capture?scope_id=session";

#[tokio::test]
async fn current_capture_selects_exact_files_without_changing_history_or_downloads() {
    // Causes C1 no capture; C2 complete A/B/A scans; C3 selected File deleted;
    // C4 observed empty; C5 another Workspace/Session filter; C6 HTTP mutation.
    // Rules R1 C1 -> null (unknown); R2 C2 -> exact revision/ids and original A
    // bytes, while Files lists both versions; R3 C3 -> keep A id but download404,
    // never fall back to B; R4 C4 -> non-null empty; R5 C5 -> unknown, no leak;
    // R6 C6 -> 405 and unchanged capture. No Session/runtime or user acceptance
    // state is created by a File filter. All reads use the real File application.
    let resources = support::resources::ephemeral_resources();
    let files = resources.files();
    let router = mounted(files.clone());
    let publisher = resources.artifact_publisher::<()>();
    assert_eq!(
        serde_json::from_slice::<Value>(
            &call(&router, "GET", URI, Some("workspace"))
                .await
                .expect(StatusCode::OK)
        )
        .unwrap(),
        json!({"scope_id":"session","capture":null}),
        "R1"
    );
    let mut ids = Vec::new();
    for (index, bytes) in [b"A", b"B", b"A"].into_iter().enumerate() {
        let digest = awaken_resource_contract::content_id(bytes);
        let file = publisher
            .publish(ArtifactPublication {
                workspace_id: "workspace".into(),
                session_id: "session".into(),
                logical_path: "report.txt".into(),
                mime_type: "text/plain".into(),
                effect_id: awaken_resource_contract::harvest_idempotency_key(
                    "session",
                    "report.txt",
                    &digest,
                ),
                content_id: digest,
                bytes: bytes.to_vec(),
                idempotency_scope: None,
                fence: None,
            })
            .await
            .unwrap()
            .record;
        ids.push(file.id.clone());
        files
            .replace_artifact_capture(ReplaceArtifactCapture {
                workspace_id: "workspace".into(),
                session_id: "session".into(),
                expected_revision: index as u64,
                file_ids: vec![file.id.clone()],
            })
            .await
            .unwrap();
        let body = call(&router, "GET", URI, Some("workspace"))
            .await
            .expect_for(StatusCode::OK, "R2");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"scope_id":"session","capture":{"revision":(index+1).to_string(),"file_ids":[file.id]}}),
            "R2"
        );
        assert_eq!(
            call(
                &router,
                "GET",
                &format!("/v1/files/{}/content", file.id),
                Some("workspace")
            )
            .await
            .expect(StatusCode::OK),
            bytes.to_vec(),
            "R2 bytes"
        );
    }
    assert_eq!(ids[0], ids[2]);
    assert_ne!(ids[0], ids[1]);
    let history: Value = serde_json::from_slice(
        &call(
            &router,
            "GET",
            "/v1/files?scope_id=session",
            Some("workspace"),
        )
        .await
        .expect(StatusCode::OK),
    )
    .unwrap();
    assert_eq!(history["data"].as_array().unwrap().len(), 2, "R2 history");
    let stable = call(&router, "GET", URI, Some("workspace"))
        .await
        .expect(StatusCode::OK);
    for method in ["POST", "PUT", "DELETE"] {
        assert_eq!(
            call(&router, method, URI, Some("workspace")).await.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "R6"
        );
    }
    assert_eq!(
        call(&router, "GET", URI, Some("workspace"))
            .await
            .expect(StatusCode::OK),
        stable,
        "R6 unchanged"
    );
    for (uri, scope) in [
        (URI, "foreign"),
        (
            "/v1/awaken/files/output-capture?scope_id=foreign",
            "workspace",
        ),
    ] {
        let body = call(&router, "GET", uri, Some(scope))
            .await
            .expect_for(StatusCode::OK, "R5");
        assert!(
            serde_json::from_slice::<Value>(&body).unwrap()["capture"].is_null(),
            "R5"
        );
    }
    assert_eq!(
        call(
            &router,
            "DELETE",
            &format!("/v1/files/{}", ids[0]),
            Some("workspace")
        )
        .await
        .status(),
        StatusCode::OK,
        "R3"
    );
    assert_eq!(
        call(&router, "GET", URI, Some("workspace"))
            .await
            .expect(StatusCode::OK),
        stable,
        "R3 selected tombstone retained"
    );
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("/v1/files/{}/content", ids[0]),
            Some("workspace")
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
        "R3"
    );
    files
        .replace_artifact_capture(ReplaceArtifactCapture {
            workspace_id: "workspace".into(),
            session_id: "session".into(),
            expected_revision: 3,
            file_ids: vec![],
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(
            &call(&router, "GET", URI, Some("workspace"))
                .await
                .expect(StatusCode::OK)
        )
        .unwrap(),
        json!({"scope_id":"session","capture":{"revision":"4","file_ids":[]}}),
        "R4"
    );
}

struct ReadOnlyCaptureCatalog {
    value: Option<ArtifactCapture>,
    fail: bool,
    reads: AtomicUsize,
}
#[async_trait::async_trait]
impl FileCatalog for ReadOnlyCaptureCatalog {
    async fn artifact_capture(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<ArtifactCapture>, FileCatalogError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(FileCatalogError::Storage("injected outage".into()))
        } else {
            Ok(self.value.clone())
        }
    }
    async fn replace_artifact_capture(
        &self,
        _: awaken_resource_contract::ReplaceArtifactCapture,
    ) -> Result<awaken_resource_contract::ReplaceArtifactCaptureOutcome, FileCatalogError> {
        unreachable!("read route cannot replace captures")
    }
    async fn create_file(
        &self,
        _: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
        unreachable!("read route cannot write")
    }
    async fn get_file(
        &self,
        _: &str,
        _: &str,
        _: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        unreachable!("capture does not guess metadata")
    }
    async fn list_files(
        &self,
        _: &str,
        _: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        unreachable!("capture is not a history scan")
    }
    async fn list_files_including_deleted(
        &self,
        _: &str,
        _: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        unreachable!("capture is not a deleted history scan")
    }
    async fn mark_file_deleted(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        unreachable!("read route cannot delete")
    }
    async fn active_size_bytes(&self, _: &str) -> Result<u64, FileCatalogError> {
        unreachable!("capture has no new quota authority")
    }
}
#[tokio::test]
async fn capture_read_rejects_missing_scope_bad_queries_outages_and_corrupt_evidence() {
    // C1 missing/empty Workspace ->404 before catalog; C2 missing/empty/duplicate
    // or unknown query ->400 before catalog; C3 corrupt catalog scope/revision/ids
    // or outage ->503, never unknown/success; C4 max i64 revision -> exact decimal
    // string without JS rounding. Mutation and metadata methods are panic traps.
    let exact = ArtifactCapture {
        workspace_id: "workspace".into(),
        session_id: "session".into(),
        revision: i64::MAX as u64,
        file_ids: vec!["a".into()],
    };
    for cause in [
        "exact",
        "missing_workspace",
        "empty_workspace",
        "missing_query",
        "empty_query",
        "duplicate_query",
        "unknown_query",
        "workspace",
        "session",
        "zero",
        "ids",
        "outage",
    ] {
        let mut value = exact.clone();
        match cause {
            "workspace" => value.workspace_id = "foreign".into(),
            "session" => value.session_id = "foreign".into(),
            "zero" => value.revision = 0,
            "ids" => value.file_ids = vec!["a".into(), "a".into()],
            _ => (),
        };
        let catalog = Arc::new(ReadOnlyCaptureCatalog {
            value: Some(value),
            fail: cause == "outage",
            reads: AtomicUsize::new(0),
        });
        let app = Arc::new(awaken_resource_application::FileApplication::new(
            Arc::new(awaken_file_store::InMemoryFileStore::new()),
            catalog.clone(),
            Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap()),
        ));
        let router = mounted(app);
        let scope = match cause {
            "missing_workspace" => None,
            "empty_workspace" => Some(" "),
            _ => Some("workspace"),
        };
        let uri = match cause {
            "missing_query" => "/v1/awaken/files/output-capture",
            "empty_query" => "/v1/awaken/files/output-capture?scope_id=",
            "duplicate_query" => "/v1/awaken/files/output-capture?scope_id=session&scope_id=other",
            "unknown_query" => "/v1/awaken/files/output-capture?scope_id=session&extra=true",
            _ => URI,
        };
        let expected = match cause {
            "exact" => 200,
            "missing_workspace" | "empty_workspace" => 404,
            "missing_query" | "empty_query" | "duplicate_query" | "unknown_query" => 400,
            _ => 503,
        };
        let body = call(&router, "GET", uri, scope)
            .await
            .expect_for(StatusCode::from_u16(expected).unwrap(), cause);
        assert_eq!(
            catalog.reads.load(Ordering::SeqCst),
            usize::from(expected == 200 || expected == 503),
            "{cause} I/O"
        );
        if cause == "exact" {
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["capture"]["revision"],
                "9223372036854775807"
            );
        }
    }
}
