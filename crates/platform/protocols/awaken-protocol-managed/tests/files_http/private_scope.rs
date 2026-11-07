//! Workspace credentials are insufficient for Session-scoped private output.
use super::*;
use awaken_session_contract::{SessionPrivateBinding, SessionRepository, SessionRepositoryReads};
use std::sync::Arc;

pub(super) async fn session_file_owner() -> (
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
    Arc<awaken_session_application::SessionQueries>,
) {
    let repo = support::session_repository("File scope owner").await;
    for (id, private) in [("deleted-session", false), ("private-session", true)] {
        let template = awaken_protocol_managed_testkit::sample_persisted(id);
        let mut root = awaken_session_contract::PersistedSession::frozen_with_budget(
            id,
            template.frozen_baseline().unwrap().clone(),
            Default::default(),
            Default::default(),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        if private {
            root = SessionPrivateBinding::new(
                "org-a".into(),
                "desk".into(),
                "alice".into(),
                "test".into(),
                1,
            )
            .unwrap()
            .bind_fresh(root)
            .unwrap();
        }
        let key =
            awaken_session_contract::SessionMutationPayload::default_create_idempotency(&root);
        repo.create("test", root, key, Vec::new()).await.unwrap();
    }
    let runtime = Arc::new(awaken_protocol_managed_testkit::RehydrateFake::default());
    let selected = awaken_session_execution_contract::SessionExecutionRoles::colocated(
        runtime.clone(),
        runtime,
    );
    let queries = Arc::new(awaken_session_application::SessionQueries::new(
        repo.clone(),
        &selected,
        None,
    ));
    (repo, queries)
}

async fn output(
    resources: &awaken_resource_application::ResourcesApplication,
    id: &str,
    scope: &str,
    created_at: &str,
) {
    let authorities = resources.authorities();
    let blob_id = authorities.file_store().put(id.as_bytes()).await.unwrap();
    authorities
        .file_catalog()
        .create_file(FileRecord {
            private_binding: None,
            id: id.into(),
            workspace_id: "test".into(),
            blob_id,
            filename: format!("{id}.txt"),
            mime_type: "text/plain".into(),
            size_bytes: id.len() as u64,
            created_at: created_at.into(),
            expires_at: None,
            downloadable: true,
            scope_id: Some(scope.into()),
            logical_path: Some(format!("{id}.txt")),
            harvest_key: None,
            artifact_idempotency_scope: None,
            deleted: false,
        })
        .await
        .unwrap();
}

async fn selected_surface(
    app: &Router,
    method: &str,
    path: &str,
    beta_header: bool,
    selector: &str,
) -> axum::response::Response {
    let path = if selector.is_empty() {
        path.to_owned()
    } else {
        let separator = if path.contains('?') { '&' } else { '?' };
        format!("{path}{separator}{}", selector.trim_end_matches('&'))
    };
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    if beta_header {
        request
            .headers_mut()
            .insert("anthropic-beta", "files-api-2025-04-14".parse().unwrap());
    }
    app.clone().oneshot(request).await.unwrap()
}

async fn check_private_file_reads(app: &Router) {
    use awaken_protocol_managed::AdmittedPrivateSessionRequest;

    for (org, application, account, workspace, revision, admitted) in [
        ("org-a", "desk", "alice", "test", 1, true),
        ("org-b", "desk", "alice", "test", 1, false),
        ("org-a", "other-app", "alice", "test", 1, false),
        ("org-a", "desk", "bob", "test", 1, false),
        ("org-a", "desk", "alice", "other-workspace", 1, false),
        ("org-a", "desk", "alice", "test", 2, false),
    ] {
        let binding = SessionPrivateBinding::new(
            org.into(),
            application.into(),
            account.into(),
            workspace.into(),
            revision,
        )
        .unwrap();
        let private = app.clone().layer(axum::Extension(
            AdmittedPrivateSessionRequest::from_admitted(binding),
        ));
        for (id, bytes) in [
            ("file_private", "file_private"),
            ("file_private_input", "file_ordinary"),
        ] {
            for suffix in ["", "/content"] {
                let response = selected_surface(
                    &private,
                    "GET",
                    &format!("/v1/files/{id}{suffix}"),
                    false,
                    "",
                )
                .await;
                let expected = if admitted {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                };
                assert_eq!(
                    response.status(),
                    expected,
                    "{account}/{workspace}/{revision}/{id}{suffix}"
                );
                if admitted && suffix == "/content" {
                    assert_eq!(
                        axum::body::to_bytes(response.into_body(), 1024)
                            .await
                            .unwrap(),
                        bytes.as_bytes()
                    );
                }
            }
        }
        for id in ["file_ordinary", "file_unknown"] {
            assert_eq!(
                selected_surface(
                    &private,
                    "GET",
                    &format!("/v1/files/{id}/content"),
                    false,
                    ""
                )
                .await
                .status(),
                StatusCode::NOT_FOUND
            );
        }
    }
}

#[tokio::test]
async fn private_upload_replays_only_within_the_same_current_personal_binding() {
    use awaken_protocol_managed::AdmittedPrivateSessionRequest;

    let resources = support::resources::ephemeral_resources().await;
    let (_repo, queries) = session_file_owner().await;
    let app = awaken_protocol_managed::resources_router(
        awaken_protocol_managed::ResourcesRouterInput::new(
            &resources,
            Arc::new(awaken_resource_contract::UnavailableRepositoryCredentialValidator),
        )
        .with_session_reads(queries),
    );
    let for_person = |org: &str, account: &str| {
        let binding =
            SessionPrivateBinding::new(org.into(), "desk".into(), account.into(), "test".into(), 1)
                .unwrap();
        app.clone().layer(axum::Extension(
            AdmittedPrivateSessionRequest::from_admitted(binding),
        ))
    };
    let a = for_person("org-a", "alice");
    let b = for_person("org-b", "bob");
    assert_eq!(
        upload(&a, "private.txt", b"a").await.status(),
        StatusCode::BAD_REQUEST
    );
    let a_first = upload_with_key(&a, "private.txt", b"a", Some("same-key"))
        .await
        .expect(StatusCode::OK);
    let a_retry = upload_with_key(&a, "private.txt", b"a", Some("same-key"))
        .await
        .expect(StatusCode::OK);
    let b_first = upload_with_key(&b, "private.txt", b"bb", Some("same-key"))
        .await
        .expect(StatusCode::OK);
    let id_a = a_first["id"].as_str().unwrap();
    let id_b = b_first["id"].as_str().unwrap();
    assert_eq!(a_retry["id"], id_a);
    assert_ne!(id_a, id_b);
    for (owner, other, id, size) in [(&a, &b, id_a, 1), (&b, &a, id_b, 2)] {
        let path = format!("/v1/files/{id}");
        let metadata = get(owner, &path).await.expect(StatusCode::OK);
        let metadata: Value = serde_json::from_slice(&metadata).unwrap();
        assert_eq!(metadata["id"], id);
        assert_eq!(metadata["size_bytes"], size);
        for addressed in [path.clone(), format!("{path}/content")] {
            assert_eq!(get(other, &addressed).await.status(), StatusCode::NOT_FOUND);
            assert_eq!(get(&app, &addressed).await.status(), StatusCode::NOT_FOUND);
        }
    }
}

#[tokio::test]
async fn native_session_visibility_protects_every_file_surface_before_pagination_bytes_and_delete()
{
    let resources = support::resources::ephemeral_resources().await;
    let (repo, queries) = session_file_owner().await;
    output(
        &resources,
        "file_private",
        "private-session",
        "2026-01-03T00:00:00Z",
    )
    .await;
    output(
        &resources,
        "file_unknown",
        "unknown-session",
        "2026-01-02T00:00:00Z",
    )
    .await;
    output(
        &resources,
        "file_ordinary",
        "deleted-session",
        "2026-01-01T00:00:00Z",
    )
    .await;
    // A personal input has no source Session. Its original File provenance
    // must still hide it before every ordinary Files projection and effect.
    let catalog = resources.authorities().file_catalog();
    let mut input = catalog
        .get_file("test", "file_ordinary", false)
        .await
        .unwrap()
        .unwrap();
    input.id = "file_private_input".into();
    input.created_at = "2026-01-04T00:00:00Z".into();
    input.scope_id = None;
    input.logical_path = None;
    input.private_binding = Some(
        awaken_resource_contract::PrivateContentBinding::new(
            "org-a".into(),
            "desk".into(),
            "alice".into(),
            "test".into(),
            1,
        )
        .unwrap(),
    );
    catalog.create_file(input).await.unwrap();
    let app = awaken_protocol_managed::resources_router(
        awaken_protocol_managed::ResourcesRouterInput::new(
            &resources,
            Arc::new(awaken_resource_contract::UnavailableRepositoryCredentialValidator),
        )
        .with_session_reads(queries),
    );
    for (beta_header, selector) in [(false, ""), (false, "beta=true&"), (true, "")] {
        for (method, path) in [
            ("GET", "/v1/files/file_private"),
            ("GET", "/v1/files/file_private/content"),
            ("DELETE", "/v1/files/file_private"),
            ("GET", "/v1/files/file_unknown"),
            ("GET", "/v1/files/file_private_input"),
            ("GET", "/v1/files/file_private_input/content"),
            ("DELETE", "/v1/files/file_private_input"),
        ] {
            let response = selected_surface(&app, method, path, beta_header, selector).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        }
        let response =
            selected_surface(&app, "GET", "/v1/files?limit=1", beta_header, selector).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["data"].as_array().unwrap().len(), 1);
        assert_eq!(body["data"][0]["id"], "file_ordinary");
        assert!(
            !body
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
        assert!(body.get("next_page").is_none_or(Value::is_null));
    }
    assert_eq!(
        get(&app, "/v1/files/file_ordinary/content")
            .await
            .expect(StatusCode::OK),
        b"file_ordinary"
    );
    check_private_file_reads(&app).await;
    let private = resources
        .authorities()
        .file_catalog()
        .get_file("test", "file_private", true)
        .await
        .unwrap()
        .unwrap();
    assert!(!private.deleted);
    assert!(
        !catalog
            .get_file("test", "file_private_input", true)
            .await
            .unwrap()
            .unwrap()
            .deleted
    );
    assert!(
        SessionPrivateBinding::of(&repo.get(&"private-session".into()).await.unwrap()).is_some()
    );
    // An unavailable owner must not reuse an earlier Workspace-visible answer.
    repo.close().await;
    assert_eq!(
        get(&app, "/v1/files/file_ordinary/content").await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn standalone_files_router_cannot_disclose_session_output_without_its_owner_port() {
    let resources = support::resources::ephemeral_resources().await;
    let authorities = resources.authorities();
    let blob_id = authorities
        .file_store()
        .put(b"private result")
        .await
        .unwrap();
    authorities
        .file_catalog()
        .create_file(FileRecord {
            private_binding: None,
            id: "file_private_output".into(),
            workspace_id: "test".into(),
            blob_id,
            filename: "private.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 14,
            created_at: "2026-01-01T00:00:00Z".into(),
            expires_at: None,
            downloadable: true,
            scope_id: Some("private-session".into()),
            logical_path: Some("private.txt".into()),
            harvest_key: None,
            artifact_idempotency_scope: None,
            deleted: false,
        })
        .await
        .unwrap();
    let app = files_router(resources.files(), authorities.clock());
    upload(&app, "ordinary.txt", b"ordinary input")
        .await
        .expect(StatusCode::OK);
    for suffix in ["", "/content"] {
        assert_eq!(
            get(&app, &format!("/v1/files/file_private_output{suffix}"))
                .await
                .status(),
            StatusCode::NOT_FOUND,
            "Workspace-only route cannot discharge Session ownership: {suffix}"
        );
    }
    let body = get(&app, "/v1/files?limit=1").await.expect(StatusCode::OK);
    let page: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"][0]["filename"], "ordinary.txt");
}
