// @formal evidence session_time.utc_projection_domain_is_total_exact_and_idempotent
//! The memory-store API (`/v1/memory_stores`, ADR-0038/0053) end-to-end through its
//! real axum router, driven over `tower::ServiceExt::oneshot`. Covers the store
//! aggregate (create / retrieve / list / update / delete / archive), the `memories`
//! subresource (create / list / retrieve / update with a `content_sha256`
//! precondition / delete), and the `memory_versions` log (list / retrieve / redact),
//! plus the fail-closed error arms (unknown store, unknown memory, bad path).
//!
//! The router is built over the canonical Resources application component and its
//! in-memory path-addressed repository — the same ports a durable deployment wires.

use awaken_protocol_managed::memory_stores_router;
use awaken_protocol_managed::types::memory::AuthenticatedMemoryActor;
use awaken_resource_contract::MemoryActor;
use awaken_runtime_contract::runs::scope::WorkspaceScope;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

mod support;

async fn router() -> Router {
    let resources = support::resources::ephemeral_resources().await;
    memory_stores_router(&resources)
}

#[tokio::test]
async fn memory_creation_replays_a_lost_response_and_rejects_changed_payloads() {
    async fn create(
        router: &Router,
        workspace: &str,
        key: &str,
        body: Value,
    ) -> support::Call<serde_json::Value> {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/memory_stores")
            .header("content-type", "application/json")
            .header("idempotency-key", key)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        request
            .extensions_mut()
            .insert(WorkspaceScope(workspace.into()));
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        support::Call::new(status, serde_json::from_slice(&bytes).unwrap())
    }
    let router = router().await;
    let body = json!({"name": "Lost response", "description": "Keep exactly one", "metadata": {"owner": "workshop"}});
    // The first POST reaches the real application. Discard its response as a
    // disconnected client would, then submit the exact request again.
    let first = create(&router, "test", "request-1", body.clone())
        .await
        .expect(StatusCode::OK);
    let retry = create(&router, "test", "request-1", body.clone()).await;
    assert_eq!(first, retry.expect(StatusCode::OK));
    let inventory = call_scoped(&router, "test", "GET", "/v1/memory_stores", None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(inventory["data"].as_array().unwrap().len(), 1);
    assert!(
        inventory.to_string().find("creation_request").is_none(),
        "internal receipts stay out of the public resource"
    );
    let mut changed = body.clone();
    changed["description"] = json!("Different request");
    let conflict = create(&router, "test", "request-1", changed)
        .await
        .expect(StatusCode::CONFLICT);
    assert_eq!(conflict["error"]["type"], "conflict_error");
    let other = create(&router, "other", "request-1", body.clone())
        .await
        .expect(StatusCode::OK);
    assert_ne!(first["id"], other["id"]);
    for key in ["", " ", &"x".repeat(256)] {
        assert_eq!(
            create(&router, "test", key, body.clone()).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    let inventory = call_scoped(&router, "test", "GET", "/v1/memory_stores", None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(inventory["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn retained_copy_candidates_use_store_visibility_and_explicit_content_view() {
    use awaken_resource_contract::{MemoryCopyBase, MemoryCopyConflict};
    // RC-API: store={owned,foreign,missing,archived}, view={basic,full,invalid},
    // operation={write-empty,delete}, page={valid,zero}. Owned+valid returns
    // stable identities, with content only under full; foreign/missing/archived
    // is 404. Invalid view/page is 400; reads never mutate heads or history.
    // This is an Awaken extension, not an assertion about the Anthropic schema.
    let resources = support::resources::ephemeral_resources().await;
    let preservation = resources.authorities().memory_copy_conflict_preservation();
    let content = resources.authorities().memory_management_content();
    let router = memory_stores_router(&resources);
    let store = create_store(&router, "conflicts").await;
    let base = MemoryCopyBase {
        id: "original".into(),
        version: 1,
        content_sha256: "a".repeat(64),
    };
    let candidates = [
        MemoryCopyConflict {
            path: "/a".into(),
            base: Some(base.clone()),
            content: Some(String::new()),
        },
        MemoryCopyConflict {
            path: "/b".into(),
            base: Some(base),
            content: None,
        },
    ];
    for candidate in &candidates {
        preservation
            .preserve_copy_conflict(&store, candidate)
            .await
            .unwrap();
    }
    let url = format!("/v1/awaken/memory-stores/{store}/copy-conflicts");
    // RC-API namespace: Awaken owns this extension; the vendor namespace must
    // remain absent even for an authorized owner of an existing Store.
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("/v1/memory_stores/{store}/copy_conflicts"),
            None
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
    );
    let basic = call(&router, "GET", &url, None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(basic["data"].as_array().unwrap().len(), 2);
    assert!(
        basic["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["content"].is_null())
    );
    let full = call(&router, "GET", &format!("{url}?view=full"), None)
        .await
        .expect(StatusCode::OK);
    for candidate in &candidates {
        let row = full["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == candidate.id())
            .unwrap();
        assert_eq!(row["content"], json!(candidate.content));
        assert_eq!(
            row["proposed_operation"],
            if candidate.content.is_some() {
                "write"
            } else {
                "delete"
            }
        );
    }
    let page = call(&router, "GET", &format!("{url}?limit=1"), None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        call_scoped(&router, "foreign", "GET", &url, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &router,
            "GET",
            "/v1/awaken/memory-stores/missing/copy-conflicts",
            None
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    for suffix in ["?view=invalid", "?limit=0"] {
        assert_eq!(
            call(&router, "GET", &format!("{url}{suffix}"), None)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert!(
        content
            .snapshots()
            .snapshot_heads(&store)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        content
            .versions()
            .list_versions(&store)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        call(
            &router,
            "POST",
            &format!("/v1/memory_stores/{store}/archive"),
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        call(&router, "GET", &url, None).await.status(),
        StatusCode::NOT_FOUND
    );
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> support::Call<serde_json::Value> {
    let builder = Request::builder().method(method).uri(uri);
    let mut req = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&v).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    req.extensions_mut()
        .insert(WorkspaceScope("test".to_string()));
    req.extensions_mut()
        .insert(AuthenticatedMemoryActor(MemoryActor::ApiActor {
            api_key_id: "api_test".into(),
        }));
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    support::Call::new(status, value)
}

#[tokio::test]
async fn resource_routes_require_a_preselected_workspace() {
    let router = router().await;
    for (method, body) in [
        ("GET", Body::empty()),
        ("POST", Body::from(r#"{"name":"must-not-exist"}"#)),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/v1/memory_stores")
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
    }

    let listed = call(&router, "GET", "/v1/memory_stores", None)
        .await
        .expect(StatusCode::OK);
    assert!(listed["data"].as_array().unwrap().is_empty());
}

async fn call_scoped(
    router: &Router,
    workspace: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> support::Call<serde_json::Value> {
    let builder = Request::builder().method(method).uri(uri);
    let mut request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    request
        .extensions_mut()
        .insert(WorkspaceScope(workspace.to_string()));
    request
        .extensions_mut()
        .insert(AuthenticatedMemoryActor(MemoryActor::ApiActor {
            api_key_id: "api_test".into(),
        }));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    support::Call::new(status, value)
}

/// Create a store over HTTP and return its minted id.
async fn create_store(router: &Router, name: &str) -> String {
    let v = call(
        router,
        "POST",
        "/v1/memory_stores",
        Some(json!({ "name": name })),
    )
    .await
    .expect_for(StatusCode::OK, "create store");
    v["id"].as_str().expect("minted store id").to_string()
}

// Test design: store_lifecycle_create_get_list_update_archive_delete
// Cause/effect graph: create establishes a MemoryStore aggregate; reads and updates project it;
// archive hides it from live queries; delete removes the terminal aggregate and its addressability.
// Decision table: absent=create/404; active=read/update/archive; archived=filtered; deleted=404.
#[tokio::test]
async fn store_lifecycle_create_get_list_update_archive_delete() {
    let router = router().await;

    // Create with the SDK body (name/description/metadata) mints a durable id.
    let created = call(
        &router,
        "POST",
        "/v1/memory_stores",
        Some(json!({ "name": "notes", "description": "my notes", "metadata": { "team": "core" } })),
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["type"], "memory_store");
    assert_eq!(created["name"], "notes");
    assert_eq!(created["metadata"]["team"], "core");
    assert_eq!(created["archived_at"], Value::Null);

    // Retrieve carries only the governed definition; mutable data has one surface,
    // `/memories`, shared with execution.
    let got = call(&router, "GET", &format!("/v1/memory_stores/{id}"), None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(got["name"], "notes");
    assert!(got.get("size_bytes").is_none());
    assert!(got.get("content").is_none());

    // List surfaces the non-archived store.
    let list = call(&router, "GET", "/v1/memory_stores", None)
        .await
        .expect(StatusCode::OK);
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == id.as_str()),
        "list surfaces the store: {list}"
    );

    // Update: patch description (empty clears) + metadata (string upsert, null delete).
    let updated = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{id}"),
        Some(json!({ "description": "updated", "metadata": { "team": null, "owner": "alice" } })),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(updated["description"], "updated");
    assert_eq!(updated["metadata"]["owner"], "alice");
    assert!(
        updated["metadata"].get("team").is_none(),
        "null metadata value deletes the key: {updated}"
    );

    // Archive stamps archived_at and drops the store from the listing.
    let archived = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{id}/archive"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_ne!(archived["archived_at"], Value::Null);
    let list = call(&router, "GET", "/v1/memory_stores", None)
        .await
        .expect(StatusCode::OK);
    assert!(
        !list["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == id.as_str()),
        "an archived store is not listed: {list}"
    );

    // Delete a fresh store is a soft archive → receipt.
    let id2 = create_store(&router, "trash").await;
    let receipt = call(&router, "DELETE", &format!("/v1/memory_stores/{id2}"), None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(receipt["type"], "memory_store_deleted");
    assert_eq!(receipt["id"], id2.as_str());
}

#[tokio::test]
async fn unknown_store_is_fail_closed_on_every_verb() {
    let router = router().await;
    let missing = "memstore_does_not_exist";

    // GET on a store with no Resource Registry identity → 404.
    let _v = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{missing}"),
        None,
    )
    .await
    .expect_for(StatusCode::NOT_FOUND, "expected status");

    // update / delete / archive on an unknown identity all 404.
    for (method, suffix, body) in [
        ("POST", "", Some(json!({ "description": "x" }))),
        ("DELETE", "", None),
        ("POST", "/archive", None),
    ] {
        let _ = call(
            &router,
            method,
            &format!("/v1/memory_stores/{missing}{suffix}"),
            body,
        )
        .await
        .expect_for(StatusCode::NOT_FOUND, "on unknown store");
    }
}

#[tokio::test]
async fn workspace_and_lifecycle_are_intrinsic_resource_guards() {
    let resources = support::resources::ephemeral_resources().await;
    let router = memory_stores_router(&resources);
    let created = call_scoped(
        &router,
        "workspace-a",
        "POST",
        "/v1/memory_stores",
        Some(json!({"name": "private"})),
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();

    let list = call_scoped(&router, "workspace-b", "GET", "/v1/memory_stores", None)
        .await
        .expect(StatusCode::OK);
    assert!(list["data"].as_array().unwrap().is_empty());
    for suffix in ["", "/memories", "/memory_versions"] {
        let _ = call_scoped(
            &router,
            "workspace-b",
            "GET",
            &format!("/v1/memory_stores/{id}{suffix}"),
            None,
        )
        .await
        .expect_for(StatusCode::NOT_FOUND, "cross-workspace");
    }

    let _ = call_scoped(
        &router,
        "workspace-a",
        "POST",
        &format!("/v1/memory_stores/{id}/archive"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    for suffix in ["/memories", "/memory_versions"] {
        let _ = call_scoped(
            &router,
            "workspace-a",
            "GET",
            &format!("/v1/memory_stores/{id}{suffix}"),
            None,
        )
        .await
        .expect_for(StatusCode::NOT_FOUND, "archived");
    }
}

// Test design: memory_crud_with_precondition_and_version_log
// Cause/effect graph: a path-addressed Memory head and immutable versions advance through CAS;
// list/retrieve/redact observe the same history while archive/delete retire only the selected head.
// Decision table: fresh precondition=advance; stale=409; missing parent/path/version=404; redact=immutable marker.
#[tokio::test]
async fn memory_crud_with_precondition_and_version_log() {
    let router = router().await;
    let store = create_store(&router, "mem-host").await;

    // Create a memory (path-addressed head in the durable store).
    let mem = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories"),
        Some(json!({ "path": "/a.md", "content": "hello" })),
    )
    .await
    .expect_for(StatusCode::OK, "expected status");
    let mid = mem["id"].as_str().unwrap().to_string();
    let sha = mem["content_sha256"].as_str().unwrap().to_string();
    let created_at = mem["created_at"].clone();
    assert_eq!(mem["path"], "/a.md");
    assert_eq!(
        mem["content"],
        Value::Null,
        "Managed Agents create defaults to the basic projection"
    );
    assert_eq!(mem["content_size_bytes"], 5);
    let created_version_id = mem["memory_version_id"].as_str().unwrap().to_string();

    // Causes: operation is create/list/retrieve/update/version-list/version-retrieve;
    // view is omitted/basic/full/invalid; stored content is present.
    // Constraints: create/list/update default basic; retrieve defaults full; full
    // list caps its page size at 20; invalid view is rejected before mutation.
    // Effects: basic projects null content while preserving identity/hash/size;
    // full projects content; invalid returns 400 and leaves the head unchanged.
    // Decision table: R1 list full -> content; R2 list omitted/basic -> null;
    // R3 retrieve omitted/full -> content; R4 retrieve basic -> null; R5 update
    // omitted/basic -> mutation with null response; R6 update full -> mutation
    // with content; R7 version list/retrieve follow their list/retrieve defaults;
    // R8 any invalid view -> 400 without a write.
    let full = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories?view=full"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(full["data"][0]["content"], "hello");
    let basic = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        basic["data"][0]["content"],
        Value::Null,
        "basic view elides content"
    );
    let explicit_basic = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories?view=basic"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(explicit_basic["data"][0]["content"], Value::Null, "R2");

    // Retrieve by id.
    let got = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(got["content"], "hello");
    let got_basic = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories/{mid}?view=basic"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(got_basic["content"], Value::Null, "R4");

    // Causes: C1 an older SDK uses POST; C2 a canonical client uses PATCH; C3
    // the supplied complete generation is stale or current. Constraint: both verbs
    // enter one update handler and therefore one CAS/version-history authority.
    // Effects: E1 stale generations conflict without mutation; E2 a current token
    // commits exactly one new version. Decision rules: C1+C3(stale) -> E1;
    // C2+C3(current) -> E2. Covering opposite verbs proves route aliases cannot
    // fork aggregate semantics.
    let conflict = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        Some(json!({ "content": "new", "precondition": {
            "type":"content_sha256", "content_sha256": "0".repeat(64)
        } })),
    )
    .await
    .expect_for(StatusCode::CONFLICT, "expected status");
    assert_eq!(
        conflict["error"]["type"],
        "memory_precondition_failed_error"
    );

    // The matching precondition (the live sha) succeeds and bumps the version id.
    let updated = call(
        &router,
        "PATCH",
        &format!("/v1/memory_stores/{store}/memories/{mid}?view=full"),
        Some(json!({ "content": "world", "precondition": {
            "type":"content_sha256", "content_sha256": sha
        } })),
    )
    .await
    .expect_for(StatusCode::OK, "expected status");
    assert_eq!(updated["content"], "world");
    let updated_version_id = updated["memory_version_id"].as_str().unwrap();
    assert_ne!(updated_version_id, created_version_id);

    // An idempotent update exercises the default basic projection without
    // appending a third version.
    let updated_basic = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        Some(json!({
            "content": "world",
            "precondition": { "type":"content_sha256",
                "content_sha256": updated["content_sha256"] }
        })),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(updated_basic["content"], Value::Null, "R5");
    assert_eq!(updated_basic["memory_version_id"], updated_version_id);
    // R6 the SDK contract: a stale precondition whose requested content and
    // path already equal the stored state answers 200 without a new version;
    // R7 a malformed hash is refused before any write; R8 the internal
    // complete-generation witness is not a wire shape.
    let stale_but_same = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        Some(json!({ "content": "world", "precondition": {
            "type":"content_sha256", "content_sha256": "0".repeat(64)
        } })),
    )
    .await
    .expect_for(StatusCode::OK, "R6");
    assert_eq!(
        stale_but_same["memory_version_id"], updated_version_id,
        "R6 no new version"
    );
    let _ = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        Some(json!({ "content": "other", "precondition": {
            "type":"content_sha256", "content_sha256": "not-a-hash"
        } })),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "R7");
    let _ = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        Some(json!({ "content": "other", "precondition": {
            "type":"generation", "memory_id": mid, "version": 1,
            "content_sha256": updated["content_sha256"]
        } })),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "R8");

    // Metamorphic list/read rule: changing the head content changes updated
    // state only. Both views must be projected from one frozen head generation;
    // neither may rewrite created_at or pair the new hash with old content.
    let after_update = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories?view=full"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    let listed = after_update["data"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == mid))
        .expect("updated memory remains listed");
    assert_eq!(listed["created_at"], created_at);
    assert_eq!(listed["content"], "world");
    assert_eq!(listed["content_sha256"], updated["content_sha256"]);
    assert_eq!(listed["memory_version_id"], updated_version_id);

    // Wire-projection decision table: operation=create/retrieve/update/list,
    // view=basic/full. Every observed partition below exposes the official
    // Memory fields only. Internal generation stays in the store's CAS owner;
    // the stale/current precondition rules above still exercise that authority.
    // Exact field sets catch accidental exposure as well as required-field loss.
    let expected_fields = std::collections::BTreeSet::from([
        "id",
        "type",
        "created_at",
        "updated_at",
        "memory_store_id",
        "memory_version_id",
        "path",
        "content",
        "content_sha256",
        "content_size_bytes",
    ]);
    for wire in [
        &mem,
        &got,
        &got_basic,
        &updated,
        &updated_basic,
        &full["data"][0],
        &basic["data"][0],
        listed,
    ] {
        assert_eq!(
            wire.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            expected_fields,
            "one official Memory projection across operations and views",
        );
    }

    // The version log now carries a `created` then a `modified` row.
    let versions = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    let ops: Vec<&str> = versions["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["operation"].as_str().unwrap())
        .collect();
    assert_eq!(
        ops,
        vec!["created", "modified"],
        "version history: {versions}"
    );
    assert!(
        versions["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|version| version["content"] == Value::Null),
        "R7 version list defaults to basic"
    );
    let full_versions = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions?view=full"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(full_versions["data"][0]["content"], "hello", "R7");
    assert_eq!(full_versions["data"][1]["content"], "world", "R7");
    // Actor cause/effect decision table: A1 authenticated API create/update ->
    // immutable versions carry api_actor; A2 read-only list -> no mutation;
    // A3 actor filter match/mismatch -> included/empty; A4 authenticated redact
    // -> redacted_by without changing created_by.
    assert_eq!(
        full_versions["data"][0]["created_by"]["type"], "api_actor",
        "A1"
    );
    assert_eq!(
        full_versions["data"][0]["created_by"]["api_key_id"], "api_test",
        "A1"
    );
    let actor_match = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions?api_key_id=api_test"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(actor_match["data"].as_array().unwrap().len(), 2, "A3");
    let actor_miss = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions?api_key_id=other"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert!(actor_miss["data"].as_array().unwrap().is_empty(), "A3");
    let first_vid = versions["data"][0]["id"].as_str().unwrap().to_string();
    // Version-id cause/effect rules: V1 create atomically appends one real
    // version -> the Memory head projects that exact id; V2 CAS update appends
    // another -> the updated head projects the new log id. A synthetic API id
    // would fail both equalities.
    assert_eq!(first_vid, created_version_id, "V1");
    assert_eq!(
        versions["data"][1]["id"].as_str().unwrap(),
        updated_version_id,
        "V2"
    );

    // Retrieve a single version.
    let ver = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions/{first_vid}"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(ver["operation"], "created");
    assert_eq!(ver["content"], "hello");
    let basic_ver = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions/{first_vid}?view=basic"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(basic_ver["content"], Value::Null, "R7");

    for route in [
        format!("/v1/memory_stores/{store}/memories?view=invalid"),
        format!("/v1/memory_stores/{store}/memories/{mid}?view=invalid"),
        format!("/v1/memory_stores/{store}/memory_versions?view=invalid"),
        format!("/v1/memory_stores/{store}/memory_versions/{first_vid}?view=invalid"),
    ] {
        assert_eq!(
            call(&router, "GET", &route, None).await.status(),
            StatusCode::BAD_REQUEST,
            "R8 {route}"
        );
    }
    assert_eq!(
        call(
            &router,
            "POST",
            &format!("/v1/memory_stores/{store}/memories?view=invalid"),
            Some(json!({ "path": "/must-not-exist.md", "content": "x" })),
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
        "R8 invalid create view"
    );
    assert_eq!(
        call(
            &router,
            "POST",
            &format!("/v1/memory_stores/{store}/memories/{mid}?view=invalid"),
            Some(json!({ "content": "must-not-persist" })),
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
        "R8 invalid update view"
    );
    let unchanged = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(unchanged["content"], "world", "R8 rejects before mutation");
    let unchanged_list = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories?view=full"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert!(
        unchanged_list["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|memory| memory["path"] != "/must-not-exist.md"),
        "R8 invalid create view rejects before mutation"
    );

    // Redact a version: stamp redacted_at + drop content.
    let redacted = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memory_versions/{first_vid}/redact"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_ne!(redacted["redacted_at"], Value::Null);
    assert_eq!(redacted["content"], Value::Null, "redaction drops content");
    assert_eq!(redacted["created_by"]["api_key_id"], "api_test", "A4");
    assert_eq!(redacted["redacted_by"]["api_key_id"], "api_test", "A4");

    // Conditional-delete causal graph: a stale expected generation must leave
    // head/history untouched; the live version+SHA atomically removes the exact
    // id/head and appends one actor-attributed deleted version. The immutable id
    // comes from the route, closing delete/recreate and same-content ABA.
    let stale_delete = call(
        &router,
        "DELETE",
        &format!(
            "/v1/memory_stores/{store}/memories/{mid}?expected_content_sha256={}",
            "0".repeat(64)
        ),
        None,
    )
    .await
    .expect_for(StatusCode::CONFLICT, "expected status");
    assert_eq!(
        stale_delete["error"]["type"],
        "memory_precondition_failed_error"
    );
    let after_stale = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(after_stale["content"], "world");
    let versions_after_stale = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(versions_after_stale["data"].as_array().unwrap().len(), 2);

    // A malformed hash is refused before any write; the internal version
    // witness is not a wire parameter.
    let _ = call(
        &router,
        "DELETE",
        &format!("/v1/memory_stores/{store}/memories/{mid}?expected_content_sha256=not-a-hash"),
        None,
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "malformed hash");
    // Matching delete → receipt + exactly one `deleted` version row.
    let receipt = call(
        &router,
        "DELETE",
        &format!(
            "/v1/memory_stores/{store}/memories/{mid}?expected_content_sha256={}",
            updated["content_sha256"].as_str().unwrap()
        ),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(receipt["type"], "memory_deleted");
    // The head is gone: retrieve by id is now 404. One request answers both
    // the status and the diagnostic; the second identical GET only existed
    // because the first discarded its status.
    let _after = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        None,
    )
    .await
    .expect_for(StatusCode::NOT_FOUND, "deleted memory");
    let versions_after_delete = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    let deleted = versions_after_delete["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|version| version["operation"] == "deleted")
        .expect("matching delete appends a version");
    assert_eq!(deleted["created_by"]["api_key_id"], "api_test");
}

#[tokio::test]
async fn memory_error_arms_are_fail_closed() {
    let router = router().await;
    let store = create_store(&router, "errs").await;

    // A create without a `path` is a 400.
    let _ = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories"),
        Some(json!({ "content": "x" })),
    )
    .await
    .expect(StatusCode::BAD_REQUEST);

    // A create against an UNKNOWN store is 404 (the mount-blob existence check).
    let _ = call(
        &router,
        "POST",
        "/v1/memory_stores/memstore_missing/memories",
        Some(json!({ "path": "/a.md", "content": "x" })),
    )
    .await
    .expect(StatusCode::NOT_FOUND);

    // Retrieve / update / delete an unknown memory id in a real store → 404.
    for method in ["GET", "POST", "DELETE"] {
        let body = (method == "POST").then(|| json!({ "content": "x" }));
        let _ = call(
            &router,
            method,
            &format!("/v1/memory_stores/{store}/memories/mem_missing"),
            body,
        )
        .await
        .expect_for(StatusCode::NOT_FOUND, "unknown memory");
    }

    // Version endpoints on an unknown store → 404; an unknown version id in a real
    // store → 404.
    let _ = call(
        &router,
        "GET",
        "/v1/memory_stores/memstore_missing/memory_versions",
        None,
    )
    .await
    .expect(StatusCode::NOT_FOUND);
    let _ = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions/memver_nope"),
        None,
    )
    .await
    .expect(StatusCode::NOT_FOUND);
    let _ = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memory_versions/memver_nope/redact"),
        None,
    )
    .await
    .expect(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn memory_store_router_does_not_reintroduce_a_parallel_store_configuration() {
    // Cause/effect decision table: R1 official Store and subresource paths have
    // an owner; R2 the former Store-policy endpoint and R3 its historical
    // config-version endpoint have no owner (404). Recall/extraction settings
    // belong to ordinary Agent plugin configuration, not a second Store API.
    let router = router().await;
    let store = create_store(&router, "compatible-only").await;
    assert_ne!(
        call(&router, "GET", "/v1/memory_stores", None)
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "R1"
    );
    for suffix in ["/config", "/config_versions/1"] {
        assert_eq!(
            call(
                &router,
                "GET",
                &format!("/v1/memory_stores/{store}{suffix}"),
                None,
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
            "removed extension {suffix}"
        );
    }
}

#[tokio::test]
async fn store_time_filters_match_wire_precision_before_pagination_and_lifecycle_filtering() {
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RegisterMemoryStore,
        ResourceState, ResourceTimestamps,
    };
    // C1 two distinct visible milliseconds within one second; C2 Active/Suspended/
    // Archived/Deleted; C3 include_archived false/true; C4 equal inclusive bounds;
    // C5 limit=1 and cursor. E1 exact public millisecond selection; E2 only allowed
    // lifecycle states; E3 no page omissions/duplicates. Cross C1*C2*C3, then drain
    // every page. Seed through the canonical Registry port with literal timestamps
    // so the former second-truncating implementation deterministically fails.
    let resources = support::resources::ephemeral_resources().await;
    let registry = resources.authorities().resource_registry();
    for millis in [100_u64, 101] {
        for (label, state) in [
            ("active", ResourceState::Active),
            ("suspended", ResourceState::Suspended),
            ("archived", ResourceState::Archived),
            ("deleted", ResourceState::Deleted),
        ] {
            let id = format!("store-{millis}-{label}");
            registry
                .register_memory_store(RegisterMemoryStore {
                    definition: MemoryStoreDefinition {
                        id: id.clone().into(),
                        workspace_id: "test".into(),
                        name: id.clone(),
                        description: String::new(),
                        metadata: Default::default(),
                        state,
                        current_config_version: ConfigVersion::INITIAL,
                        timestamps: ResourceTimestamps::created(millis * 1_000_000 + 999_999),
                    },
                    initial_config: MemoryStoreConfigVersion {
                        memory_store_id: id.into(),
                        version: ConfigVersion::INITIAL,
                        retention_policy: Default::default(),
                    },
                })
                .await
                .unwrap();
        }
    }
    let router = memory_stores_router(&resources);
    for include_archived in [false, true] {
        for (millis, wire) in [
            (100, "1970-01-01T00:00:00.1Z"),
            (101, "1970-01-01T00:00:00.101Z"),
        ] {
            let mut ids = Vec::new();
            let mut cursor = String::new();
            loop {
                let page = call(&router, "GET", &format!(
                    "/v1/memory_stores?created_at%5Bgte%5D={wire}&created_at%5Blte%5D={wire}&include_archived={include_archived}&limit=1{cursor}"), None).await.expect_for(StatusCode::OK, "expected status");
                for row in page["data"].as_array().unwrap() {
                    assert_eq!(row["created_at"], wire);
                    ids.push(row["id"].as_str().unwrap().to_string());
                }
                assert!(ids.len() <= 3, "cursor cannot repeat a visible row");
                match page["next_page"].as_str() {
                    Some(next) => cursor = format!("&page={next}"),
                    None => break,
                }
            }
            let mut expected = vec![
                format!("store-{millis}-active"),
                format!("store-{millis}-suspended"),
            ];
            if include_archived {
                expected.push(format!("store-{millis}-archived"));
            }
            ids.sort();
            expected.sort();
            assert_eq!(ids, expected);
        }
    }
}

// Test design: R6b -- the SDK idempotency contract across both conflict origins.
//
// R6 (covered above) reaches the already-current answer through the handler's
// own pre-read: a stale precondition whose requested content and path already
// equal the stored state answers 200. That path is only reachable when the
// winning write landed *before* this request loaded the head.
//
// The interleaving R6's pre-read cannot see is two callers that observed the
// same head. Both bind to the generation they read, the winner commits that
// exact content first, and the loser's CAS then fails on generation staleness.
// The store reports that as `MemErr::Conflict`, carrying the live head it
// observed inside its own transaction -- a head whose content already equals
// what the loser asked for. Answering 409 there contradicts R6 for a state
// that is already achieved, and makes the public outcome depend on scheduling.
//
// Causes: C1 the precondition matches the head this request loaded (so this is
// not R6's stale-precondition path); C2 a concurrent writer commits the exact
// requested content against the same base first; C3 the requested path is
// unchanged. Effect: 200 carrying the winner's version, and no third version
// row -- the repository invariant that idempotent updates append nothing.
//
// The winner is committed by a decorator inside `update_head_as`, so the store
// produces a genuine stale-generation Conflict rather than a fabricated error.
struct RaceLosingRepository {
    backend: std::sync::Arc<awaken_memory_store::VolatileMemoryRepository>,
    inner: awaken_resource_contract::MemoryManagementContent,
    interposed: std::sync::atomic::AtomicBool,
    /// What the winning caller commits. `None` means it requested the same
    /// content as the loser, which is the already-current row.
    winner_content: Option<String>,
}

// Every required role belongs to one backend. Only the management Writer
// interposes a genuine concurrent CAS winner; other roles forward unchanged.
impl RaceLosingRepository {
    fn new(
        clock: std::sync::Arc<dyn awaken_time::WallClock>,
        winner_content: Option<String>,
    ) -> Self {
        let backend =
            std::sync::Arc::new(awaken_memory_store::VolatileMemoryRepository::new(clock));
        Self {
            inner: awaken_resource_contract::MemoryManagementContent::new(backend.clone()),
            backend,
            interposed: std::sync::atomic::AtomicBool::new(false),
            winner_content,
        }
    }
}

macro_rules! forward_memory_role {
    ($role:ident { $(fn $method:ident($($arg:ident: $ty:ty),*) -> $result:ty;)* }) => {
        #[async_trait::async_trait]
        impl awaken_resource_contract::$role for RaceLosingRepository {
            $(async fn $method(&self, $($arg: $ty),*) -> $result {
                awaken_resource_contract::$role::$method(self.backend.as_ref(), $($arg),*).await
            })*
        }
    };
}

forward_memory_role!(MemoryQueries {
    fn list(store: &str, prefix: &str) -> Result<Vec<awaken_resource_contract::MemoryEntry>, awaken_resource_contract::MemErr>;
    fn get_by_path(store: &str, path: &str) -> Result<Option<awaken_resource_contract::Memory>, awaken_resource_contract::MemErr>;
});
forward_memory_role!(MemoryFilesystem {
    fn list(store: &str, prefix: &str) -> Result<Vec<awaken_resource_contract::MemoryEntry>, awaken_resource_contract::MemErr>;
    fn get_by_path(store: &str, path: &str) -> Result<Option<awaken_resource_contract::Memory>, awaken_resource_contract::MemErr>;
    fn create_as(store: &str, path: &str, content: &str, actor: Option<&MemoryActor>) -> Result<awaken_resource_contract::Memory, awaken_resource_contract::MemErr>;
    fn update_head_as(store: &str, base: &awaken_resource_contract::MemoryGeneration, content: &str, target_path: Option<&str>, actor: Option<&MemoryActor>) -> Result<awaken_resource_contract::Memory, awaken_resource_contract::MemErr>;
    fn rename_as(store: &str, from: &str, to: &str, actor: Option<&MemoryActor>) -> Result<awaken_resource_contract::Memory, awaken_resource_contract::MemErr>;
    fn delete_by_path_as(store: &str, path: &str, actor: Option<&MemoryActor>) -> Result<(), awaken_resource_contract::MemErr>;
});
forward_memory_role!(MemoryCopyWriteback {
    fn create_as(store: &str, path: &str, content: &str, actor: Option<&MemoryActor>) -> Result<awaken_resource_contract::Memory, awaken_resource_contract::MemErr>;
    fn update_materialized(store: &str, path: &str, base: &awaken_resource_contract::MemoryGeneration, content: &str) -> Result<awaken_resource_contract::Memory, awaken_resource_contract::MemErr>;
    fn delete_if_match_as(store: &str, path: &str, base: &awaken_resource_contract::MemoryGeneration, actor: Option<&MemoryActor>) -> Result<bool, awaken_resource_contract::MemErr>;
});
forward_memory_role!(MemoryCopyConflictPreservation {
    fn preserve_copy_conflict(store: &str, candidate: &awaken_resource_contract::MemoryCopyConflict) -> Result<(), awaken_resource_contract::MemErr>;
});
forward_memory_role!(MemoryContentPurge {
    fn purge_store(store: &str) -> Result<awaken_resource_contract::MemoryPurgeSummary, awaken_resource_contract::MemErr>;
});

// These HTTP CAS rows do not mount content. Invalidation coupling has its own
// real-backend conformance assertions; no notification claim is made here.
struct UnmountedMemoryInvalidator;
impl awaken_resource_contract::MemoryInvalidator for UnmountedMemoryInvalidator {
    fn publish(&self, _store: &str, _path: &str) {}
}

#[async_trait::async_trait]
#[cfg(test)]
impl awaken_resource_contract::MemoryWriter for RaceLosingRepository {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<awaken_resource_contract::Memory, awaken_resource_contract::MemErr> {
        self.inner
            .writer()
            .create_as(store, path, content, actor)
            .await
    }
    async fn update_head_as(
        &self,
        store: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<awaken_resource_contract::Memory, awaken_resource_contract::MemErr> {
        if !self
            .interposed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            // The winning caller observed the same head. Its write advances the
            // generation this call still holds, at the original path.
            let winner = self.winner_content.as_deref().unwrap_or(content);
            self.inner
                .writer()
                .update_head_as(store, base, winner, None, actor)
                .await?;
        }
        self.inner
            .writer()
            .update_head_as(store, base, content, target_path, actor)
            .await
    }
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), awaken_resource_contract::MemErr> {
        self.inner
            .writer()
            .delete_by_path_as(store, path, actor)
            .await
    }
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, awaken_resource_contract::MemErr> {
        self.inner
            .writer()
            .delete_if_match_as(store, path, base, actor)
            .await
    }
}

#[async_trait::async_trait]
#[cfg(test)]
impl awaken_resource_contract::MemorySnapshotSource for RaceLosingRepository {
    async fn snapshot_heads(
        &self,
        store: &str,
    ) -> Result<Vec<awaken_resource_contract::Memory>, awaken_resource_contract::MemErr> {
        self.inner.snapshots().snapshot_heads(store).await
    }
}

#[async_trait::async_trait]
#[cfg(test)]
impl awaken_resource_contract::MemoryVersionQueries for RaceLosingRepository {
    async fn list_versions(
        &self,
        store: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryVersion>, awaken_resource_contract::MemErr>
    {
        self.inner.versions().list_versions(store).await
    }
}

#[async_trait::async_trait]
#[cfg(test)]
impl awaken_resource_contract::MemoryVersionRedaction for RaceLosingRepository {
    async fn redact_version_as(
        &self,
        store: &str,
        version: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Option<awaken_resource_contract::MemoryVersion>, awaken_resource_contract::MemErr>
    {
        self.inner
            .redaction()
            .redact_version_as(store, version, actor)
            .await
    }
}

#[async_trait::async_trait]
#[cfg(test)]
impl awaken_resource_contract::MemoryCopyConflictQueries for RaceLosingRepository {
    async fn list_copy_conflicts(
        &self,
        store: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryCopyConflict>, awaken_resource_contract::MemErr>
    {
        self.inner
            .conflict_queries()
            .list_copy_conflicts(store)
            .await
    }
}

#[tokio::test]
async fn store_found_conflicts_answer_the_contract_only_when_already_at_the_requested_state() {
    // Decision table over the head the store observed transactionally:
    // same content + unchanged path => 200 with the winner's version and no new
    // row; a different head content, or a requested move away from the head's
    // path, remains a precise 409. The third row proves the answer is not a
    // blanket acceptance of every stale-generation conflict.
    for (winner_content, requested_path, expected, rule) in [
        (
            None,
            None,
            StatusCode::OK,
            "R6b already at the requested state",
        ),
        (
            Some("a different edit"),
            None,
            StatusCode::CONFLICT,
            "R6c a different head content is still a conflict",
        ),
        (
            None,
            Some("/moved.md"),
            StatusCode::CONFLICT,
            "R6d an unsatisfied path move is still a conflict",
        ),
    ] {
        let resources = support::resources::ephemeral_resources().await;
        let authorities = resources.authorities();
        let resources = awaken_resource_application::ResourcesApplication::new(
            awaken_resource_application::ResourceAuthorities::new(
                authorities.resource_registry(),
                authorities.file_store(),
                authorities.file_catalog(),
                std::sync::Arc::new(
                    awaken_resource_application::memory::InvalidatingMemoryRepository::new(
                        std::sync::Arc::new(RaceLosingRepository::new(
                            authorities.clock(),
                            winner_content.map(str::to_string),
                        )),
                        std::sync::Arc::new(UnmountedMemoryInvalidator),
                    ),
                ),
                authorities.skill_store(),
                authorities.reclamation(),
                authorities.clock(),
            ),
        );
        let router = memory_stores_router(&resources);
        let store = create_store(&router, "race-host").await;
        let created = call(
            &router,
            "POST",
            &format!("/v1/memory_stores/{store}/memories"),
            Some(json!({ "path": "/race.md", "content": "base" })),
        )
        .await
        .expect_for(StatusCode::OK, "create the contended head");
        let mid = created["id"].as_str().unwrap().to_string();
        let live_sha = created["content_sha256"].as_str().unwrap().to_string();

        // C1 the precondition is the live head's hash, so `bind` succeeds and
        // this request is fenced on the generation it read -- R6's pre-read
        // path cannot fire and the conflict can only come from the store.
        let mut body = json!({
            "content": "same edit",
            "precondition": { "type": "content_sha256", "content_sha256": live_sha }
        });
        if let Some(path) = requested_path {
            body["path"] = json!(path);
        }
        let losing = call(
            &router,
            "POST",
            &format!("/v1/memory_stores/{store}/memories/{mid}"),
            Some(body),
        )
        .await
        .expect_for(expected, rule);

        let versions = call(
            &router,
            "GET",
            &format!("/v1/memory_stores/{store}/memory_versions"),
            None,
        )
        .await
        .expect(StatusCode::OK);
        assert_eq!(
            versions["data"].as_array().expect("version rows").len(),
            2,
            "{rule}: idempotent and refused updates both append nothing"
        );
        if expected == StatusCode::OK {
            // Compare against the live head rather than a row position, so the
            // rule is asserted independently of version-log ordering.
            let head = call(
                &router,
                "GET",
                &format!("/v1/memory_stores/{store}/memories/{mid}?view=full"),
                None,
            )
            .await
            .expect(StatusCode::OK);
            assert_eq!(
                losing["memory_version_id"], head["memory_version_id"],
                "{rule}: the answer carries the winner's committed version"
            );
            assert_eq!(head["content"], "same edit", "{rule}: the winner stands");
        }
    }
}
