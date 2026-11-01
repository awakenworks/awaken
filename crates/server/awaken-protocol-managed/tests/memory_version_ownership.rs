//! T25-C01 (归属与权限): a memory version id is addressable only through the
//! store, and the workspace, that own it. Driven through the real
//! `memory_stores_router` over the canonical Resources application.
//!
//! Oracles (groups-4.json order):
//! - O0 a foreign version can be neither read nor redacted;
//! - O1 the refusal does not leak whether the foreign resource exists
//!   (foreign and nonexistent answers are byte-identical);
//! - O2 every refusal changes nothing (both stores' full version logs are
//!   unchanged, the foreign content is still readable by its owner).
//!
//! | Row | Caller workspace | Route store | Version id | Expected |
//! |---|---|---|---|---|
//! | X1 | A | B (own) | A's version | 404, same body as X2 |
//! | X2 | A | B (own) | nonexistent | 404 `memory_version not found` |
//! | X3 | B (other tenant) | C (B's own) | A's version | 404, same body as X4 |
//! | X4 | B | C | nonexistent | 404 |
//! | X5 | B | A's store | A's version | 404, same body as X6 |
//! | X6 | B | nonexistent store | nonexistent | 404 `memory_store not found` |
//! | P1 | A | A (owner) | A's version | 200 redact (the id was real) |
//!
//! Negative control: a repository whose redaction looks a version up by id
//! alone, without restricting it to the route's store, must be detected by
//! the same checker (`store_unscoped_version_lookup_is_detected`).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use awaken_protocol_managed::memory_stores_router;
use awaken_protocol_managed::types::memory::AuthenticatedMemoryActor;
use awaken_resource_contract::{
    MemErr, MemoryActor, MemoryVersion, MemoryVersionQueries as _, MemoryVersionRedaction,
};
use awaken_tenancy::WorkspaceScope;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

mod support;

const TENANT_A: &str = "t25-workspace-a";
const TENANT_B: &str = "t25-workspace-b";
const SECRET: &str = "t25 secret content of store A";

async fn call(
    router: &Router,
    workspace: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> support::Call<Value> {
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
            api_key_id: "api_t25".into(),
        }));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    support::Call::new(
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn create_store(router: &Router, workspace: &str, name: &str) -> String {
    call(
        router,
        workspace,
        "POST",
        "/v1/memory_stores",
        Some(json!({ "name": name })),
    )
    .await
    .expect_for(StatusCode::OK, "create store")["id"]
        .as_str()
        .expect("minted store id")
        .to_string()
}

/// Create one memory and return the version id its creation minted.
async fn create_memory(
    router: &Router,
    workspace: &str,
    store: &str,
    path: &str,
    content: &str,
) -> String {
    call(
        router,
        workspace,
        "POST",
        &format!("/v1/memory_stores/{store}/memories"),
        Some(json!({ "path": path, "content": content })),
    )
    .await
    .expect_for(StatusCode::OK, "create memory")["memory_version_id"]
        .as_str()
        .expect("minted version id")
        .to_string()
}

async fn full_log(router: &Router, workspace: &str, store: &str) -> Value {
    call(
        router,
        workspace,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions?view=full"),
        None,
    )
    .await
    .expect_for(StatusCode::OK, "read a full version log")
}

#[derive(Clone)]
struct Fixture {
    store_a: String,
    store_b: String,
    store_c: String,
    version_a: String,
}

async fn fixture(router: &Router) -> Fixture {
    let store_a = create_store(router, TENANT_A, "t25-owner").await;
    let store_b = create_store(router, TENANT_A, "t25-sibling").await;
    let store_c = create_store(router, TENANT_B, "t25-other-tenant").await;
    let version_a = create_memory(router, TENANT_A, &store_a, "/secret.md", SECRET).await;
    // Both other stores carry their own history, so "not in this log" is not
    // an artefact of an empty log.
    create_memory(router, TENANT_A, &store_b, "/sibling.md", "sibling").await;
    create_memory(router, TENANT_B, &store_c, "/tenant.md", "tenant").await;
    Fixture {
        store_a,
        store_b,
        store_c,
        version_a,
    }
}

/// O2 predicate over the owner's full log: exactly the one created version,
/// with its content and no redaction stamp.
fn owner_version_is_intact(log: &Value, version: &str) -> bool {
    let rows = log["data"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    rows.len() == 1
        && rows[0]["id"] == version
        && rows[0]["content"] == SECRET
        && rows[0]["redacted_at"] == Value::Null
}

/// The T25-C01 checker. Every assertion names its row and oracle, so a
/// divergent backend fails it with the exact rule it broke.
async fn assert_version_ownership(router: &Router, fixture: &Fixture) {
    let Fixture {
        store_a,
        store_b,
        store_c,
        version_a,
    } = fixture;
    let before = (
        full_log(router, TENANT_A, store_a).await,
        full_log(router, TENANT_A, store_b).await,
        full_log(router, TENANT_B, store_c).await,
    );
    let missing_version = "memver_t25_does_not_exist";
    let missing_store = "memstore_t25_does_not_exist";
    // (row, workspace, foreign route store, control store)
    let rows = [
        ("X1/X2", TENANT_A, store_b.as_str(), store_b.as_str()),
        ("X3/X4", TENANT_B, store_c.as_str(), store_c.as_str()),
        ("X5/X6", TENANT_B, store_a.as_str(), missing_store),
    ];
    for (row, workspace, store, control_store) in rows {
        for (verb, method, suffix) in [("read", "GET", ""), ("redact", "POST", "/redact")] {
            let foreign = call(
                router,
                workspace,
                method,
                &format!("/v1/memory_stores/{store}/memory_versions/{version_a}{suffix}"),
                None,
            )
            .await
            .expect_for(
                StatusCode::NOT_FOUND,
                &format!("{row}/O0 foreign {verb} is refused"),
            );
            let absent = call(
                router,
                workspace,
                method,
                &format!(
                    "/v1/memory_stores/{control_store}/memory_versions/{missing_version}{suffix}"
                ),
                None,
            )
            .await
            .expect_for(
                StatusCode::NOT_FOUND,
                &format!("{row} control: a nonexistent {verb} target"),
            );
            assert!(
                !foreign.to_string().contains(SECRET),
                "{row}/O0 foreign {verb} disclosed the content: {foreign}"
            );
            assert_eq!(
                foreign, absent,
                "{row}/O1 foreign {verb} is indistinguishable from a nonexistent one"
            );
        }
    }
    // The foreign id is not listed by a store that does not own it.
    for (workspace, store) in [(TENANT_A, store_b), (TENANT_B, store_c)] {
        let log = full_log(router, workspace, store).await.to_string();
        assert!(
            !log.contains(version_a.as_str()) && !log.contains(SECRET),
            "O0 store {store} lists the foreign version: {log}"
        );
    }
    let after = (
        full_log(router, TENANT_A, store_a).await,
        full_log(router, TENANT_A, store_b).await,
        full_log(router, TENANT_B, store_c).await,
    );
    assert_eq!(
        after, before,
        "O2 every refusal left every version log unchanged"
    );
    assert!(
        owner_version_is_intact(&after.0, version_a),
        "O2 the owner's version is unaltered: {}",
        after.0
    );
}

#[tokio::test]
async fn foreign_version_ids_are_inert_and_indistinguishable_from_absent_ones() {
    let resources = support::resources::ephemeral_resources().await;
    let router = memory_stores_router(&resources);
    let fixture = fixture(&router).await;

    assert_version_ownership(&router, &fixture).await;

    // P1: the id the refused rows used is a real, redactable version of its
    // owner, so every 404 above was an ownership refusal, not a typo.
    let redacted = call(
        &router,
        TENANT_A,
        "POST",
        &format!(
            "/v1/memory_stores/{}/memory_versions/{}/redact",
            fixture.store_a, fixture.version_a
        ),
        None,
    )
    .await
    .expect_for(StatusCode::OK, "P1 owner redact");
    assert_eq!(
        redacted["id"],
        fixture.version_a.as_str(),
        "P1 owner redact"
    );
    assert_ne!(redacted["redacted_at"], Value::Null, "P1 owner redact");
    assert_eq!(
        redacted["content"],
        Value::Null,
        "P1 owner redact drops content"
    );
}

// ---- Negative control: a store-unscoped version lookup ---------------------

/// The volatile backend with one deliberate divergence: redaction finds the
/// version by id across every store it has seen, ignoring the route store.
struct UnscopedRedaction {
    backend: Arc<awaken_memory_store::VolatileMemoryRepository>,
    stores: Mutex<BTreeSet<String>>,
}

macro_rules! forward_memory_role {
    ($role:ident { $(fn $method:ident($($arg:ident: $ty:ty),*) -> $result:ty;)* }) => {
        #[async_trait::async_trait]
        impl awaken_resource_contract::$role for UnscopedRedaction {
            $(async fn $method(&self, $($arg: $ty),*) -> $result {
                awaken_resource_contract::$role::$method(self.backend.as_ref(), $($arg),*).await
            })*
        }
    };
}

forward_memory_role!(MemoryQueries {
    fn list(store: &str, prefix: &str) -> Result<Vec<awaken_resource_contract::MemoryEntry>, MemErr>;
    fn get_by_path(store: &str, path: &str) -> Result<Option<awaken_resource_contract::Memory>, MemErr>;
});
forward_memory_role!(MemoryFilesystem {
    fn list(store: &str, prefix: &str) -> Result<Vec<awaken_resource_contract::MemoryEntry>, MemErr>;
    fn get_by_path(store: &str, path: &str) -> Result<Option<awaken_resource_contract::Memory>, MemErr>;
    fn create_as(store: &str, path: &str, content: &str, actor: Option<&MemoryActor>) -> Result<awaken_resource_contract::Memory, MemErr>;
    fn update_head_as(store: &str, base: &awaken_resource_contract::MemoryGeneration, content: &str, target_path: Option<&str>, actor: Option<&MemoryActor>) -> Result<awaken_resource_contract::Memory, MemErr>;
    fn rename_as(store: &str, from: &str, to: &str, actor: Option<&MemoryActor>) -> Result<awaken_resource_contract::Memory, MemErr>;
    fn delete_by_path_as(store: &str, path: &str, actor: Option<&MemoryActor>) -> Result<(), MemErr>;
});
forward_memory_role!(MemoryCopyWriteback {
    fn create_as(store: &str, path: &str, content: &str, actor: Option<&MemoryActor>) -> Result<awaken_resource_contract::Memory, MemErr>;
    fn update_materialized(store: &str, path: &str, base: &awaken_resource_contract::MemoryGeneration, content: &str) -> Result<awaken_resource_contract::Memory, MemErr>;
    fn delete_if_match_as(store: &str, path: &str, base: &awaken_resource_contract::MemoryGeneration, actor: Option<&MemoryActor>) -> Result<bool, MemErr>;
});
forward_memory_role!(MemoryCopyConflictPreservation {
    fn preserve_copy_conflict(store: &str, candidate: &awaken_resource_contract::MemoryCopyConflict) -> Result<(), MemErr>;
});
forward_memory_role!(MemoryContentPurge {
    fn purge_store(store: &str) -> Result<awaken_resource_contract::MemoryPurgeSummary, MemErr>;
});
forward_memory_role!(MemorySnapshotSource {
    fn snapshot_heads(store: &str) -> Result<Vec<awaken_resource_contract::Memory>, MemErr>;
});
forward_memory_role!(MemoryCopyConflictQueries {
    fn list_copy_conflicts(store: &str) -> Result<Vec<awaken_resource_contract::MemoryCopyConflict>, MemErr>;
});

forward_memory_role!(MemoryWriter {
    fn create_as(store: &str, path: &str, content: &str, actor: Option<&MemoryActor>) -> Result<awaken_resource_contract::Memory, MemErr>;
    fn update_head_as(store: &str, base: &awaken_resource_contract::MemoryGeneration, content: &str, target_path: Option<&str>, actor: Option<&MemoryActor>) -> Result<awaken_resource_contract::Memory, MemErr>;
    fn delete_by_path_as(store: &str, path: &str, actor: Option<&MemoryActor>) -> Result<(), MemErr>;
    fn delete_if_match_as(store: &str, path: &str, base: &awaken_resource_contract::MemoryGeneration, actor: Option<&MemoryActor>) -> Result<bool, MemErr>;
});

/// Every store whose log was ever read is a candidate the divergent
/// redaction searches; the checker reads all three logs before it probes.
#[async_trait::async_trait]
impl awaken_resource_contract::MemoryVersionQueries for UnscopedRedaction {
    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        self.stores.lock().unwrap().insert(store.to_string());
        self.backend.list_versions(store).await
    }
}

#[async_trait::async_trait]
impl MemoryVersionRedaction for UnscopedRedaction {
    async fn redact_version_as(
        &self,
        _store: &str,
        version: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        // The divergence: the route's store is ignored and the id alone selects.
        let stores: Vec<String> = self.stores.lock().unwrap().iter().cloned().collect();
        for store in stores {
            let owned = self.backend.list_versions(&store).await?;
            if owned.iter().any(|candidate| candidate.id == version) {
                return self.backend.redact_version_as(&store, version, actor).await;
            }
        }
        Ok(None)
    }
}

struct UnmountedMemoryInvalidator;
impl awaken_resource_contract::MemoryInvalidator for UnmountedMemoryInvalidator {
    fn publish(&self, _store: &str, _path: &str) {}
}

#[tokio::test]
async fn store_unscoped_version_lookup_is_detected() {
    let resources = support::resources::ephemeral_resources().await;
    let authorities = resources.authorities();
    let divergent = awaken_resource_application::ResourcesApplication::new(
        awaken_resource_application::ResourceAuthorities::new(
            authorities.resource_registry(),
            authorities.file_store(),
            authorities.file_catalog(),
            Arc::new(
                awaken_resource_application::memory::InvalidatingMemoryRepository::new(
                    Arc::new(UnscopedRedaction {
                        backend: Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                            authorities.clock(),
                        )),
                        stores: Mutex::new(BTreeSet::new()),
                    }),
                    Arc::new(UnmountedMemoryInvalidator),
                ),
            ),
            authorities.skill_store(),
            authorities.reclamation(),
            authorities.clock(),
        ),
    );
    let router = memory_stores_router(&divergent);
    let fixture = fixture(&router).await;
    let checked = {
        let (router, fixture) = (router.clone(), fixture.clone());
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::spawn(async move { assert_version_ownership(&router, &fixture).await }),
        )
        .await
        .expect("NC: the checker settles within its deadline")
    };
    let panic = checked.expect_err("NC: the checker must reject the store-unscoped lookup");
    assert!(panic.is_panic(), "NC: the checker failed by assertion");
    let payload = panic.into_panic();
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(
        message.contains("X1/X2/O0 foreign redact is refused"),
        "NC: the checker names the broken rule; it reported {message:?}"
    );
    // The divergent redaction really altered the owner's history, which the
    // same O2 predicate reports.
    let owner_log = full_log(&router, TENANT_A, &fixture.store_a).await;
    assert!(
        !owner_version_is_intact(&owner_log, &fixture.version_a),
        "NC: the O2 predicate detects the foreign redaction: {owner_log}"
    );
}
