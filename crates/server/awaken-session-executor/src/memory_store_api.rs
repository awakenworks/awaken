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
//! store's `content` / `size_bytes` are still read from that blob — while the SDK's
//! richer object model (named stores + individual memories at paths + version
//! history) is layered on top in an in-memory registry keyed by store id. The
//! legacy no-body `POST /v1/memory_stores` still works; the SDK sends a `name`.

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

/// A memory: a file at a `path` within a store, with a version history. The head
/// (`versions.last`) carries the current content.
#[derive(Clone)]
struct Memory {
    id: String,
    path: String,
    versions: Vec<MemoryVersion>,
}

impl Memory {
    fn head(&self) -> &MemoryVersion {
        self.versions
            .last()
            .expect("a memory always has >=1 version")
    }

    fn content(&self) -> String {
        self.head().content.clone().unwrap_or_default()
    }

    fn project(&self, store_id: &str) -> Value {
        let content = self.content();
        json!({
            "id": self.id,
            "type": "memory",
            "created_at": OBJECT_AT,
            "updated_at": OBJECT_AT,
            "memory_store_id": store_id,
            "memory_version_id": self.head().id,
            "path": self.path,
            "content": content,
            "content_sha256": sha256_hex(&content),
            "content_size_bytes": content.len(),
        })
    }
}

/// The SDK-side metadata + memories for one store (the mount blob is separate).
#[derive(Clone, Default)]
struct StoreMeta {
    name: String,
    description: String,
    metadata: BTreeMap<String, String>,
    archived_at: Option<String>,
    memories: BTreeMap<String, Memory>,
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
    mem_seq: AtomicU64,
    ver_seq: AtomicU64,
}

impl MemoryStoreApi {
    fn next_memory_id(&self) -> String {
        format!("mem_{:016}", self.mem_seq.fetch_add(1, Ordering::SeqCst))
    }
    fn next_version_id(&self) -> String {
        format!("memver_{:016}", self.ver_seq.fetch_add(1, Ordering::SeqCst))
    }
}

/// Mount the memory-store API over the host's mutable memory stores.
pub fn memory_stores_router(host: Arc<SharedHost>) -> Router {
    let state = Arc::new(MemoryStoreApi {
        host,
        registry: Mutex::new(BTreeMap::new()),
        mem_seq: AtomicU64::new(0),
        ver_seq: AtomicU64::new(0),
    });
    Router::new()
        .route("/v1/memory_stores", post(create_store).get(list_stores))
        .route(
            "/v1/memory_stores/:id",
            get(get_store).post(update_store).delete(delete_store),
        )
        .route("/v1/memory_stores/:id/archive", post(archive_store))
        .route(
            "/v1/memory_stores/:id/memories",
            post(create_memory).get(list_memories),
        )
        .route(
            "/v1/memory_stores/:id/memories/:mid",
            get(get_memory).post(update_memory).delete(delete_memory),
        )
        .route("/v1/memory_stores/:id/memory_versions", get(list_versions))
        .route(
            "/v1/memory_stores/:id/memory_versions/:vid",
            get(get_version),
        )
        .route(
            "/v1/memory_stores/:id/memory_versions/:vid/redact",
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
    let id = state.host.create_memory_store();
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
        memories: BTreeMap::new(),
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
    let blob = state.host.memory_get(&id);
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
        .unwrap_or_default()
        .to_string();
    let mem_id = state.next_memory_id();
    let ver_id = state.next_version_id();
    let memory = Memory {
        id: mem_id.clone(),
        path: path.to_string(),
        versions: vec![MemoryVersion {
            id: ver_id,
            memory_id: mem_id.clone(),
            operation: "created",
            content: Some(content),
            path: path.to_string(),
            redacted_at: None,
        }],
    };
    let mut registry = state.registry.lock().unwrap();
    let Some(meta) = registry.get_mut(&id) else {
        return not_found("memory_store");
    };
    let projected = memory.project(&id);
    meta.memories.insert(mem_id, memory);
    (StatusCode::OK, Json(projected)).into_response()
}

async fn list_memories(
    State(state): State<Arc<MemoryStoreApi>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let registry = state.registry.lock().unwrap();
    let Some(meta) = registry.get(&id) else {
        return not_found("memory_store");
    };
    let data: Vec<Value> = meta.memories.values().map(|m| m.project(&id)).collect();
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
    let registry = state.registry.lock().unwrap();
    match registry.get(&id).and_then(|m| m.memories.get(&mid)) {
        Some(memory) => (StatusCode::OK, Json(memory.project(&id))).into_response(),
        None => not_found("memory"),
    }
}

/// `POST /v1/memory_stores/:id/memories/:mid` — update a memory's content and/or
/// path. A `content_sha256` precondition that does not match the current head is
/// a `412` (the SDK's `memory_precondition_failed_error`). Appends a `modified`
/// version.
async fn update_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    Path((id, mid)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let mut registry = state.registry.lock().unwrap();
    let Some(memory) = registry.get_mut(&id).and_then(|m| m.memories.get_mut(&mid)) else {
        return not_found("memory");
    };
    // Optimistic precondition on the current content hash.
    if let Some(expected) = body
        .get("precondition")
        .and_then(|p| p.get("content_sha256"))
        .and_then(Value::as_str)
    {
        let current = sha256_hex(&memory.content());
        if current != expected {
            // The SDK documents a precondition mismatch as HTTP 409.
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "type": "error",
                    "error": {
                        "type": "memory_precondition_failed_error",
                        "message": "content_sha256 precondition did not match",
                    }
                })),
            )
                .into_response();
        }
    }
    let new_content = body
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| memory.content());
    let new_path = body
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| memory.path.clone());
    let ver_id = state.next_version_id();
    memory.path = new_path.clone();
    memory.versions.push(MemoryVersion {
        id: ver_id,
        memory_id: mid.clone(),
        operation: "modified",
        content: Some(new_content),
        path: new_path,
        redacted_at: None,
    });
    (StatusCode::OK, Json(memory.project(&id))).into_response()
}

async fn delete_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    Path((id, mid)): Path<(String, String)>,
) -> axum::response::Response {
    let mut registry = state.registry.lock().unwrap();
    let Some(meta) = registry.get_mut(&id) else {
        return not_found("memory_store");
    };
    if meta.memories.remove(&mid).is_some() {
        (
            StatusCode::OK,
            Json(json!({ "id": mid, "type": "memory_deleted" })),
        )
            .into_response()
    } else {
        not_found("memory")
    }
}

// ---- Version routes --------------------------------------------------------

/// All versions across the store's memories (newest last), ascending id.
fn collect_versions(meta: &StoreMeta) -> Vec<&MemoryVersion> {
    let mut versions: Vec<&MemoryVersion> = meta
        .memories
        .values()
        .flat_map(|m| m.versions.iter())
        .collect();
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
    for memory in meta.memories.values_mut() {
        if let Some(version) = memory.versions.iter_mut().find(|v| v.id == vid) {
            version.redacted_at = Some(OBJECT_AT.to_string());
            version.content = None;
            return (StatusCode::OK, Json(version.project(&id))).into_response();
        }
    }
    not_found("memory_version")
}
