//! The memory-store API (`/v1/memory_stores`) behind the ADR-0038 MemoryStore
//! resource family, aligned to the official `@anthropic-ai/sdk`
//! `beta.memoryStores.*` surface: the store (create / retrieve / update / list /
//! delete / archive), the `memories` subresource (create / retrieve / update /
//! list / delete, with a `content_sha256` precondition), and `memory_versions`
//! (retrieve / list / redact).
//!
//! Two things coexist without regression: the original **mount blob** (a store is
//! a mutable directory a session mounts read-write; the host harvests the write
//! back under the same id and it survives a restart) stays exactly as-is — the
//! store's `content` / `size_bytes` are still read from that blob. On top, the SDK's
//! richer object model: each **memory** (a path-addressed file with a
//! `content_sha256` + CAS update) is the durable [`awaken_memory_store::MemoryFs`]
//! (ADR-0053) — the same store the write-through FUSE mount projects, so memories
//! survive a restart — while the **version history** the `/memory_versions`
//! endpoints surface is an in-memory log keyed by store id. The legacy no-body
//! `POST /v1/memory_stores` still works; the SDK sends a `name`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use awaken_memory_store::MemErr;

use crate::host::SharedHost;

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// Lowercase-hex SHA-256 of the UTF-8 bytes (the `content_sha256` the SDK uses
/// for staleness checks and update preconditions).
fn sha256_hex(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// One version of a memory: an operation on its content, hashed + sized.
#[derive(Clone)]
struct MemoryVersion {
    id: String,
    memory_id: String,
    /// `"created"` | `"modified"` | `"deleted"`.
    operation: &'static str,
    content: Option<String>,
    path: String,
    redacted_at: Option<String>,
}

impl MemoryVersion {
    fn project(&self, store_id: &str) -> Value {
        let (sha, size) = match &self.content {
            Some(c) => (Some(sha256_hex(c)), Some(c.len())),
            None => (None, None),
        };
        json!({
            "id": self.id,
            "type": "memory_version",
            "created_at": OBJECT_AT,
            "memory_id": self.memory_id,
            "memory_store_id": store_id,
            "operation": self.operation,
            "content": self.content,
            "content_sha256": sha,
            "content_size_bytes": size,
            "path": self.path,
            "redacted_at": self.redacted_at,
        })
    }
}

/// Project a durable [`awaken_memory_store::Memory`] (the path-addressed head, the
/// source of truth) onto the SDK memory object.
fn project_memory(mem: &awaken_memory_store::Memory, store_id: &str) -> Value {
    let content = mem.content.clone().unwrap_or_default();
    json!({
        "id": mem.id,
        "type": "memory",
        "created_at": OBJECT_AT,
        "updated_at": OBJECT_AT,
        "memory_store_id": store_id,
        // The monotonic per-path version → a version id that changes on every write.
        "memory_version_id": format!("memver_{}_{}", mem.id, mem.version),
        "path": mem.path,
        "content": content,
        "content_sha256": mem.content_sha256,
        "content_size_bytes": mem.content_size,
    })
}

/// The SDK-side store metadata + an append-only version log. The current head of
/// each memory lives in the durable [`awaken_memory_store::MemoryFs`]; this log keeps
/// the version history the `/memory_versions` endpoints surface.
#[derive(Clone, Default)]
struct StoreMeta {
    name: String,
    description: String,
    metadata: BTreeMap<String, String>,
    archived_at: Option<String>,
    versions: Vec<MemoryVersion>,
}

impl StoreMeta {
    fn project(&self, id: &str) -> Value {
        json!({
            "id": id,
            "type": "memory_store",
            "created_at": OBJECT_AT,
            "updated_at": OBJECT_AT,
            "name": self.name,
            "description": self.description,
            "metadata": self.metadata,
            "archived_at": self.archived_at,
        })
    }
}

struct MemoryStoreApi {
    host: Arc<SharedHost>,
    registry: Mutex<BTreeMap<String, StoreMeta>>,
    ver_seq: AtomicU64,
}

impl MemoryStoreApi {
    fn next_version_id(&self) -> String {
        format!("memver_{:016}", self.ver_seq.fetch_add(1, Ordering::SeqCst))
    }
}

/// Mount the memory-store API over the host's mutable memory stores.
pub fn memory_stores_router(host: Arc<SharedHost>) -> Router {
    let state = Arc::new(MemoryStoreApi {
        host,
        registry: Mutex::new(BTreeMap::new()),
        ver_seq: AtomicU64::new(0),
    });
    Router::new()
        .route("/v1/memory_stores", post(create_store).get(list_stores))
        .route(
            "/v1/memory_stores/{id}",
            get(get_store).post(update_store).delete(delete_store),
        )
        .route("/v1/memory_stores/{id}/archive", post(archive_store))
        .route(
            "/v1/memory_stores/{id}/memories",
            post(create_memory).get(list_memories),
        )
        .route(
            "/v1/memory_stores/{id}/memories/{mid}",
            get(get_memory).post(update_memory).delete(delete_memory),
        )
        .route("/v1/memory_stores/{id}/memory_versions", get(list_versions))
        .route(
            "/v1/memory_stores/{id}/memory_versions/{vid}",
            get(get_version),
        )
        .route(
            "/v1/memory_stores/{id}/memory_versions/{vid}/redact",
            post(redact_version),
        )
        .with_state(state)
}

fn err(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn not_found(what: &str) -> axum::response::Response {
    err(StatusCode::NOT_FOUND, format!("{what} not found"))
}

// ---- Store routes ----------------------------------------------------------

/// `POST /v1/memory_stores` — create a store. The SDK sends `{name, description?,
/// metadata?}`; the legacy mount path sends no body (name defaults empty). Both
/// mint a real mount-blob store id via the host.
async fn create_store(State(state): State<Arc<MemoryStoreApi>>, body: Bytes) -> impl IntoResponse {
    let parsed: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    let id = state.host.create_memory_store().await;
    let meta = StoreMeta {
        name: parsed
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        description: parsed
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        metadata: parsed
            .get("metadata")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        archived_at: None,
        versions: Vec::new(),
    };
    let projected = meta.project(&id);
    state.registry.lock().unwrap().insert(id, meta);
    (StatusCode::OK, Json(projected))
}

/// `GET /v1/memory_stores/:id` — the SDK store object PLUS the legacy mount-blob
/// `content` / `size_bytes` (extra fields the SDK decoder ignores). Works after a
/// restart even when the in-memory registry is empty: the durable blob still
/// answers, with default metadata.
async fn get_store(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let blob = state.host.memory_get(&id).await;
    let registry = state.registry.lock().unwrap();
    let meta = registry.get(&id);
    if blob.is_none() && meta.is_none() {
        return not_found("memory_store");
    }
    let mut obj = meta.cloned().unwrap_or_default().project(&id);
    // Legacy mount-blob fields (additive; the SDK ignores them).
    let bytes = blob.unwrap_or_default();
    obj["content"] = json!(String::from_utf8_lossy(&bytes));
    obj["size_bytes"] = json!(bytes.len());
    (StatusCode::OK, Json(obj)).into_response()
}

async fn list_stores(State(state): State<Arc<MemoryStoreApi>>) -> impl IntoResponse {
    let registry = state.registry.lock().unwrap();
    let data: Vec<Value> = registry
        .iter()
        .filter(|(_, m)| m.archived_at.is_none())
        .map(|(id, m)| m.project(id))
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "data": data, "has_more": false, "next_page": null })),
    )
}

async fn update_store(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let mut registry = state.registry.lock().unwrap();
    let Some(meta) = registry.get_mut(&id) else {
        return not_found("memory_store");
    };
    // `description`: empty string clears it (SDK convention).
    if let Some(desc) = body.get("description") {
        meta.description = desc.as_str().unwrap_or_default().to_string();
    }
    // `metadata`: patch — string upserts, null deletes, omitted preserves.
    if let Some(patch) = body.get("metadata").and_then(Value::as_object) {
        for (k, v) in patch {
            match v {
                Value::Null => {
                    meta.metadata.remove(k);
                }
                Value::String(s) => {
                    meta.metadata.insert(k.clone(), s.clone());
                }
                _ => {}
            }
        }
    }
    (StatusCode::OK, Json(meta.project(&id))).into_response()
}

async fn delete_store(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let removed = state.registry.lock().unwrap().remove(&id).is_some();
    if removed {
        (
            StatusCode::OK,
            Json(json!({ "id": id, "type": "memory_store_deleted" })),
        )
            .into_response()
    } else {
        not_found("memory_store")
    }
}

async fn archive_store(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let mut registry = state.registry.lock().unwrap();
    let Some(meta) = registry.get_mut(&id) else {
        return not_found("memory_store");
    };
    meta.archived_at = Some(OBJECT_AT.to_string());
    (StatusCode::OK, Json(meta.project(&id))).into_response()
}

// ---- Memory routes ---------------------------------------------------------

/// Find a memory's current path by its id in the durable store (path-addressed, so
/// an id lookup scans the store's listing — memory stores are small).
async fn path_of(state: &MemoryStoreApi, store: &str, mid: &str) -> Option<String> {
    state
        .host
        .memory_fs
        .list(store, "/")
        .await
        .ok()?
        .into_iter()
        .find(|e| e.id == mid)
        .map(|e| e.path)
}

/// Append a version-history row for the `/memory_versions` endpoints.
fn record_version(
    state: &MemoryStoreApi,
    store: &str,
    memory_id: &str,
    path: &str,
    content: Option<String>,
    operation: &'static str,
) {
    let ver = MemoryVersion {
        id: state.next_version_id(),
        memory_id: memory_id.to_string(),
        operation,
        content,
        path: path.to_string(),
        redacted_at: None,
    };
    if let Some(meta) = state.registry.lock().unwrap().get_mut(store) {
        meta.versions.push(ver);
    }
}

fn memory_conflict() -> axum::response::Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "type": "error",
            "error": {
                "type": "memory_precondition_failed_error",
                "message": "content_sha256 precondition did not match",
            }
        })),
    )
        .into_response()
}

async fn create_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let Some(path) = body.get("path").and_then(Value::as_str) else {
        return err(StatusCode::BAD_REQUEST, "memory needs a `path`");
    };
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Store existence is checked against the DURABLE mount blob (survives a restart),
    // not the in-memory registry — so a store created before a restart still accepts
    // memories after it.
    if state.host.memory_get(&id).await.is_none() {
        return not_found("memory_store");
    }
    // The durable path-addressed store is the source of truth for the head.
    match state.host.memory_fs.create(&id, path, content).await {
        Ok(mem) => {
            record_version(
                &state,
                &id,
                &mem.id,
                path,
                Some(content.to_string()),
                "created",
            );
            (StatusCode::OK, Json(project_memory(&mem, &id))).into_response()
        }
        Err(MemErr::PathConflict(_)) => memory_conflict(),
        Err(MemErr::InvalidPath(_)) => err(StatusCode::BAD_REQUEST, "invalid memory path"),
        Err(MemErr::TooLarge) => err(StatusCode::BAD_REQUEST, "memory content too large"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn list_memories(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    // The durable store is the source of truth; no registry dependency, so a listing
    // works after a restart even though the in-memory registry is empty.
    let prefix = q.get("path_prefix").map(String::as_str).unwrap_or("/");
    let basic = q.get("view").map(String::as_str) == Some("basic");
    let entries = match state.host.memory_fs.list(&id, prefix).await {
        Ok(entries) => entries,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let mut data = Vec::with_capacity(entries.len());
    for entry in entries {
        // `basic` elides content (metadata only); `full` fetches the head content.
        let content = if basic {
            None
        } else {
            state
                .host
                .memory_fs
                .get_by_path(&id, &entry.path)
                .await
                .ok()
                .flatten()
                .and_then(|m| m.content)
        };
        data.push(json!({
            "id": entry.id,
            "type": "memory",
            "created_at": OBJECT_AT,
            "updated_at": OBJECT_AT,
            "memory_store_id": id,
            "memory_version_id": format!("memver_{}_{}", entry.id, entry.version),
            "path": entry.path,
            "content": content,
            "content_sha256": entry.content_sha256,
            "content_size_bytes": entry.content_size,
        }));
    }
    (
        StatusCode::OK,
        Json(json!({ "data": data, "has_more": false, "next_page": null })),
    )
        .into_response()
}

async fn get_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    Path((id, mid)): Path<(String, String)>,
) -> axum::response::Response {
    let Some(path) = path_of(&state, &id, &mid).await else {
        return not_found("memory");
    };
    match state.host.memory_fs.get_by_path(&id, &path).await {
        Ok(Some(mem)) => (StatusCode::OK, Json(project_memory(&mem, &id))).into_response(),
        _ => not_found("memory"),
    }
}

/// `POST /v1/memory_stores/:id/memories/:mid` — update a memory's content (and/or
/// path). A `content_sha256` precondition that does not match the durable head is a
/// `409` (the SDK's `memory_precondition_failed_error`), enforced as a compare-and-
/// swap in the store. Appends a `modified` version.
async fn update_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    Path((id, mid)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let Some(path) = path_of(&state, &id, &mid).await else {
        return not_found("memory");
    };
    let Ok(Some(current)) = state.host.memory_fs.get_by_path(&id, &path).await else {
        return not_found("memory");
    };
    // The CAS base: an explicit precondition (stale → conflict) else the live sha.
    let base_sha = body
        .get("precondition")
        .and_then(|p| p.get("content_sha256"))
        .and_then(Value::as_str)
        .unwrap_or(&current.content_sha256)
        .to_string();
    let new_content = body
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| current.content.clone().unwrap_or_default());

    let updated = match state
        .host
        .memory_fs
        .update(&id, &mid, &new_content, &base_sha)
        .await
    {
        Ok(updated) => updated,
        Err(MemErr::Conflict { .. }) => return memory_conflict(),
        Err(MemErr::TooLarge) => return err(StatusCode::BAD_REQUEST, "memory content too large"),
        Err(_) => return not_found("memory"),
    };
    // An optional path change is a rename on the store (keeps the id + open fds).
    let final_mem = match body.get("path").and_then(Value::as_str) {
        Some(new_path) if new_path != path => {
            match state.host.memory_fs.rename(&id, &path, new_path).await {
                Ok(m) => m,
                Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
            }
        }
        _ => updated,
    };
    record_version(
        &state,
        &id,
        &mid,
        &final_mem.path,
        Some(new_content),
        "modified",
    );
    (StatusCode::OK, Json(project_memory(&final_mem, &id))).into_response()
}

async fn delete_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    Path((id, mid)): Path<(String, String)>,
) -> axum::response::Response {
    let Some(path) = path_of(&state, &id, &mid).await else {
        return not_found("memory");
    };
    if state
        .host
        .memory_fs
        .delete_by_path(&id, &path)
        .await
        .is_err()
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "delete failed");
    }
    record_version(&state, &id, &mid, &path, None, "deleted");
    (
        StatusCode::OK,
        Json(json!({ "id": mid, "type": "memory_deleted" })),
    )
        .into_response()
}

// ---- Version routes --------------------------------------------------------

/// The store's version log (ascending id).
fn collect_versions(meta: &StoreMeta) -> Vec<&MemoryVersion> {
    let mut versions: Vec<&MemoryVersion> = meta.versions.iter().collect();
    versions.sort_by(|a, b| a.id.cmp(&b.id));
    versions
}

async fn list_versions(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let registry = state.registry.lock().unwrap();
    let Some(meta) = registry.get(&id) else {
        return not_found("memory_store");
    };
    let data: Vec<Value> = collect_versions(meta)
        .iter()
        .map(|v| v.project(&id))
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "data": data, "has_more": false, "next_page": null })),
    )
        .into_response()
}

async fn get_version(
    State(state): State<Arc<MemoryStoreApi>>,
    Path((id, vid)): Path<(String, String)>,
) -> axum::response::Response {
    let registry = state.registry.lock().unwrap();
    let Some(meta) = registry.get(&id) else {
        return not_found("memory_store");
    };
    match collect_versions(meta).into_iter().find(|v| v.id == vid) {
        Some(v) => (StatusCode::OK, Json(v.project(&id))).into_response(),
        None => not_found("memory_version"),
    }
}

/// `POST /v1/memory_stores/:id/memory_versions/:vid/redact` — redact a version:
/// stamp `redacted_at` and drop its content.
async fn redact_version(
    State(state): State<Arc<MemoryStoreApi>>,
    Path((id, vid)): Path<(String, String)>,
    _query: Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let mut registry = state.registry.lock().unwrap();
    let Some(meta) = registry.get_mut(&id) else {
        return not_found("memory_store");
    };
    if let Some(version) = meta.versions.iter_mut().find(|v| v.id == vid) {
        version.redacted_at = Some(OBJECT_AT.to_string());
        version.content = None;
        return (StatusCode::OK, Json(version.project(&id))).into_response();
    }
    not_found("memory_version")
}
