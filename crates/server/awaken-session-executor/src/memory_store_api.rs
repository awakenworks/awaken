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

use awaken_config_resolver::MemoryStoreDef;
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

/// Project a durable [`MemoryStoreDef`] (the identity aggregate, source of truth for a
/// store's name/description/metadata) onto the SDK memory-store object. The
/// `created_at`/`updated_at` are stable object constants (the def carries no clock);
/// `archived_at` is stamped from the def's archived flag.
fn project_def(def: &MemoryStoreDef) -> Value {
    json!({
        "id": def.id,
        "type": "memory_store",
        "created_at": OBJECT_AT,
        "updated_at": OBJECT_AT,
        "name": def.name,
        "description": def.description,
        "metadata": def.metadata,
        "archived_at": if def.archived { Some(OBJECT_AT) } else { None },
    })
}

/// A default (metadata-empty) def for `id` — used when the durable content blob answers
/// but the identity registry has no row (e.g. an id minted before the registry existed).
fn default_def(id: &str) -> MemoryStoreDef {
    MemoryStoreDef {
        id: id.to_string(),
        name: String::new(),
        description: String::new(),
        metadata: BTreeMap::new(),
        archived: false,
    }
}

struct MemoryStoreApi {
    host: Arc<SharedHost>,
    /// The durable identity registry (id/name/metadata/archived): the control-plane
    /// aggregate that survives a restart and is enumerable from the admin plane.
    registry: Arc<dyn awaken_config_resolver::MemoryStoreRegistry>,
    /// The append-only version log the `/memory_versions` endpoints surface, keyed by
    /// store id. Version history is *content* history, not identity, and is process-local
    /// (the durable head lives in `MemoryFs`), so it stays an ephemeral per-process map.
    versions: Mutex<BTreeMap<String, Vec<MemoryVersion>>>,
    ver_seq: AtomicU64,
}

impl MemoryStoreApi {
    fn next_version_id(&self) -> String {
        format!("memver_{:016}", self.ver_seq.fetch_add(1, Ordering::SeqCst))
    }
}

/// Mount the memory-store API over the host's mutable memory stores. Identity is read
/// and written through the host's [`MemoryStoreRegistry`](awaken_config_resolver::MemoryStoreRegistry)
/// (the durable admin backend when the composition root wired one, else ephemeral).
pub fn memory_stores_router(host: Arc<SharedHost>) -> Router {
    let registry = host.memory_registry();
    let state = Arc::new(MemoryStoreApi {
        host,
        registry,
        versions: Mutex::new(BTreeMap::new()),
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
    let def = MemoryStoreDef {
        id: id.clone(),
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
        archived: false,
    };
    let projected = project_def(&def);
    // Persist the identity through the durable registry (survives a restart).
    state.registry.put_memory_store(def);
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
    let def = state.registry.get_memory_store(&id);
    if blob.is_none() && def.is_none() {
        return not_found("memory_store");
    }
    let mut obj = project_def(&def.unwrap_or_else(|| default_def(&id)));
    // Legacy mount-blob fields (additive; the SDK ignores them).
    let bytes = blob.unwrap_or_default();
    obj["content"] = json!(String::from_utf8_lossy(&bytes));
    obj["size_bytes"] = json!(bytes.len());
    (StatusCode::OK, Json(obj)).into_response()
}

async fn list_stores(State(state): State<Arc<MemoryStoreApi>>) -> impl IntoResponse {
    // The registry already returns non-archived defs sorted by id.
    let data: Vec<Value> = state
        .registry
        .list_memory_stores()
        .iter()
        .map(project_def)
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
    let Some(mut def) = state.registry.get_memory_store(&id) else {
        return not_found("memory_store");
    };
    // `description`: empty string clears it (SDK convention).
    if let Some(desc) = body.get("description") {
        def.description = desc.as_str().unwrap_or_default().to_string();
    }
    // `metadata`: patch — string upserts, null deletes, omitted preserves.
    if let Some(patch) = body.get("metadata").and_then(Value::as_object) {
        for (k, v) in patch {
            match v {
                Value::Null => {
                    def.metadata.remove(k);
                }
                Value::String(s) => {
                    def.metadata.insert(k.clone(), s.clone());
                }
                _ => {}
            }
        }
    }
    let projected = project_def(&def);
    // Persist the merged identity back through the durable registry.
    state.registry.put_memory_store(def);
    (StatusCode::OK, Json(projected)).into_response()
}

async fn delete_store(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    // The registry port is put/get/list (no hard delete, mirroring `McpStore`); a delete
    // is a soft archive on the identity (the durable content blob is never destroyed
    // here either), so the store drops out of the listing. 404 when the id is unknown.
    let Some(mut def) = state.registry.get_memory_store(&id) else {
        return not_found("memory_store");
    };
    def.archived = true;
    state.registry.put_memory_store(def);
    (
        StatusCode::OK,
        Json(json!({ "id": id, "type": "memory_store_deleted" })),
    )
        .into_response()
}

async fn archive_store(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(mut def) = state.registry.get_memory_store(&id) else {
        return not_found("memory_store");
    };
    def.archived = true;
    let projected = project_def(&def);
    state.registry.put_memory_store(def);
    (StatusCode::OK, Json(projected)).into_response()
}

// ---- Memory routes ---------------------------------------------------------

/// Find a memory's current path by its id in the durable store (path-addressed, so
/// an id lookup scans the store's listing — memory stores are small).
async fn path_of(state: &MemoryStoreApi, store: &str, mid: &str) -> Option<String> {
    state
        .host
        .memory_stores
        .fs()
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
    state
        .versions
        .lock()
        .unwrap()
        .entry(store.to_string())
        .or_default()
        .push(ver);
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
    match state
        .host
        .memory_stores
        .fs()
        .create(&id, path, content)
        .await
    {
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
    let entries = match state.host.memory_stores.fs().list(&id, prefix).await {
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
                .memory_stores
                .fs()
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
    match state.host.memory_stores.fs().get_by_path(&id, &path).await {
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
    let Ok(Some(current)) = state.host.memory_stores.fs().get_by_path(&id, &path).await else {
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
        .memory_stores
        .fs()
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
            match state
                .host
                .memory_stores
                .fs()
                .rename(&id, &path, new_path)
                .await
            {
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
        .memory_stores
        .fs()
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

/// A store's version log (ascending id). Empty when the store has no recorded versions.
fn collect_versions(log: &[MemoryVersion]) -> Vec<MemoryVersion> {
    let mut versions: Vec<MemoryVersion> = log.to_vec();
    versions.sort_by(|a, b| a.id.cmp(&b.id));
    versions
}

/// Whether a store id exists (durable identity registry, else the durable content blob so
/// a store minted before the registry existed still answers).
async fn store_exists(state: &MemoryStoreApi, id: &str) -> bool {
    state.registry.get_memory_store(id).is_some() || state.host.memory_get(id).await.is_some()
}

async fn list_versions(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if !store_exists(&state, &id).await {
        return not_found("memory_store");
    }
    let log = state
        .versions
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .unwrap_or_default();
    let data: Vec<Value> = collect_versions(&log)
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
    if !store_exists(&state, &id).await {
        return not_found("memory_store");
    }
    let log = state
        .versions
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .unwrap_or_default();
    match collect_versions(&log).into_iter().find(|v| v.id == vid) {
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
    let mut versions = state.versions.lock().unwrap();
    let Some(log) = versions.get_mut(&id) else {
        return not_found("memory_version");
    };
    if let Some(version) = log.iter_mut().find(|v| v.id == vid) {
        version.redacted_at = Some(OBJECT_AT.to_string());
        version.content = None;
        return (StatusCode::OK, Json(version.project(&id))).into_response();
    }
    not_found("memory_version")
}
