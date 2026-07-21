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
//! endpoints surface is an append-only SQLite log when storage is durable (and an
//! in-memory adapter only for explicitly ephemeral deployments). The legacy no-body
//! `POST /v1/memory_stores` still works; the SDK sends a `name`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use awaken_config_resolver::MemoryStoreDef;
use awaken_memory_store::MemErr;
use awaken_tenancy::WorkspaceScope;

use crate::host::SharedHost;

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// Lowercase-hex SHA-256 of the UTF-8 bytes (the `content_sha256` the SDK uses
/// for staleness checks and update preconditions).
fn sha256_hex(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// One version of a memory: an operation on its content, hashed + sized.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct MemoryVersion {
    id: String,
    memory_id: String,
    /// `"created"` | `"modified"` | `"deleted"`.
    operation: String,
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
        workspace_id: String::new(),
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
    /// Unified resource configuration/lifecycle catalog consumed by Session
    /// resolution. It contains no authorization policy; the HTTP PEP has already
    /// supplied the trusted Workspace scope.
    catalog: Option<Arc<dyn awaken_protocol_managed::ResourceCatalog>>,
    /// Durable append-only history adapter (ephemeral only when the whole host is).
    versions: VersionRepository,
}

enum VersionRepository {
    Memory {
        logs: Mutex<BTreeMap<String, Vec<MemoryVersion>>>,
        seq: AtomicU64,
    },
    Sqlite(Mutex<rusqlite::Connection>),
}

impl VersionRepository {
    fn open() -> Self {
        let Some(dir) = std::env::var("AWAKEN_STORAGE_DIR")
            .ok()
            .filter(|dir| !dir.trim().is_empty())
            .map(std::path::PathBuf::from)
        else {
            return Self::Memory {
                logs: Mutex::new(BTreeMap::new()),
                seq: AtomicU64::new(0),
            };
        };
        Self::open_at(&dir)
    }

    fn open_at(dir: &std::path::Path) -> Self {
        std::fs::create_dir_all(dir).expect("create resource API storage directory");
        let conn = rusqlite::Connection::open(dir.join("resource-api.db"))
            .expect("open resource API database");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_versions (\
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,\
                 store_id TEXT NOT NULL,\
                 version_id TEXT UNIQUE,\
                 data TEXT NOT NULL\
             );\
             CREATE INDEX IF NOT EXISTS memory_versions_store_seq \
                 ON memory_versions(store_id, seq);",
        )
        .expect("migrate memory version log");
        Self::Sqlite(Mutex::new(conn))
    }

    fn append(&self, store: &str, mut version: MemoryVersion) {
        match self {
            Self::Memory { logs, seq } => {
                version.id = format!("memver_{:016}", seq.fetch_add(1, Ordering::SeqCst));
                logs.lock()
                    .expect("memory versions")
                    .entry(store.to_string())
                    .or_default()
                    .push(version);
            }
            Self::Sqlite(conn) => {
                let conn = conn.lock().expect("memory versions");
                conn.execute(
                    "INSERT INTO memory_versions(store_id, version_id, data) VALUES (?1, NULL, '')",
                    rusqlite::params![store],
                )
                .expect("append memory version");
                let seq = conn.last_insert_rowid();
                version.id = format!("memver_{seq:016}");
                let data = serde_json::to_string(&version).expect("encode memory version");
                conn.execute(
                    "UPDATE memory_versions SET version_id = ?1, data = ?2 WHERE seq = ?3",
                    rusqlite::params![version.id, data, seq],
                )
                .expect("finalize memory version");
            }
        }
    }

    fn list(&self, store: &str) -> Vec<MemoryVersion> {
        match self {
            Self::Memory { logs, .. } => logs
                .lock()
                .expect("memory versions")
                .get(store)
                .cloned()
                .unwrap_or_default(),
            Self::Sqlite(conn) => {
                let conn = conn.lock().expect("memory versions");
                let mut stmt = conn
                    .prepare("SELECT data FROM memory_versions WHERE store_id = ?1 ORDER BY seq")
                    .expect("prepare memory version list");
                stmt.query_map(rusqlite::params![store], |row| row.get::<_, String>(0))
                    .expect("list memory versions")
                    .map(|row| {
                        serde_json::from_str(&row.expect("read memory version"))
                            .expect("decode memory version")
                    })
                    .collect()
            }
        }
    }

    fn redact(&self, store: &str, version_id: &str) -> Option<MemoryVersion> {
        let mut version = self
            .list(store)
            .into_iter()
            .find(|version| version.id == version_id)?;
        version.redacted_at = Some(OBJECT_AT.to_string());
        version.content = None;
        match self {
            Self::Memory { logs, .. } => {
                let mut logs = logs.lock().expect("memory versions");
                *logs
                    .get_mut(store)?
                    .iter_mut()
                    .find(|candidate| candidate.id == version_id)? = version.clone();
            }
            Self::Sqlite(conn) => {
                let data = serde_json::to_string(&version).expect("encode redacted version");
                conn.lock()
                    .expect("memory versions")
                    .execute(
                        "UPDATE memory_versions SET data = ?1 WHERE store_id = ?2 AND version_id = ?3",
                        rusqlite::params![data, store, version_id],
                    )
                    .expect("redact memory version");
            }
        }
        Some(version)
    }
}

/// Mount the memory-store API over the host's mutable memory stores. Identity is read
/// and written through the host's [`MemoryStoreRegistry`](awaken_config_resolver::MemoryStoreRegistry)
/// (the durable admin backend when the composition root wired one, else ephemeral).
pub fn memory_stores_router(host: Arc<SharedHost>) -> Router {
    memory_stores_router_over(host, None)
}

/// Mount the Memory API over the same Resource Catalog used by Session
/// resolution. Composition roots that manage resources must use this variant so
/// create/archive/delete and activation share one lifecycle truth.
pub fn memory_stores_router_with_catalog(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_protocol_managed::ResourceCatalog>,
) -> Router {
    memory_stores_router_over(host, Some(catalog))
}

fn memory_stores_router_over(
    host: Arc<SharedHost>,
    catalog: Option<Arc<dyn awaken_protocol_managed::ResourceCatalog>>,
) -> Router {
    let registry = host.memory_registry();
    let state = Arc::new(MemoryStoreApi {
        host,
        registry,
        catalog,
        versions: VersionRepository::open(),
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

fn request_workspace(state: &MemoryStoreApi, scope: Option<Extension<WorkspaceScope>>) -> String {
    scope.map_or_else(
        || state.host.local_workspace().to_string(),
        |Extension(scope)| scope.0,
    )
}

// ---- Store routes ----------------------------------------------------------

/// `POST /v1/memory_stores` — create a store. The SDK sends `{name, description?,
/// metadata?}`; the legacy mount path sends no body (name defaults empty). Both
/// mint a real mount-blob store id via the host.
async fn create_store(
    State(state): State<Arc<MemoryStoreApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    body: Bytes,
) -> impl IntoResponse {
    let parsed: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    let workspace = request_workspace(&state, scope);
    let id = state.host.create_memory_store_in(&workspace).await;
    let def = MemoryStoreDef {
        id: id.clone(),
        workspace_id: workspace,
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
    state.registry.put_memory_store(def.clone());
    if let Some(catalog) = &state.catalog
        && let Err(error) = catalog.create_memory_store(
            awaken_protocol_managed::resource_plane::MemoryStoreDefinition {
                id: id.clone(),
                workspace_id: def.workspace_id.clone(),
                name: def.name.clone(),
                description: def.description.clone(),
                metadata: def.metadata.clone(),
                state: awaken_protocol_managed::resource_plane::ResourceState::Active,
                current_config_version:
                    awaken_protocol_managed::resource_plane::ConfigVersion::INITIAL,
            },
            awaken_protocol_managed::resource_plane::MemoryStoreConfigVersion {
                memory_store_id: id,
                version: awaken_protocol_managed::resource_plane::ConfigVersion::INITIAL,
                recall_policy: Default::default(),
                extraction_policy: Default::default(),
                retention_policy: Default::default(),
            },
        )
    {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("memory_store catalog write failed: {error}"),
        );
    }
    (StatusCode::OK, Json(projected)).into_response()
}

/// `GET /v1/memory_stores/:id` — the SDK store object PLUS the legacy mount-blob
/// `content` / `size_bytes` (extra fields the SDK decoder ignores). Works after a
/// restart even when the in-memory registry is empty: the durable blob still
/// answers, with default metadata.
async fn get_store(
    State(state): State<Arc<MemoryStoreApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let workspace = request_workspace(&state, scope);
    let blob = state.host.memory_get_in(&workspace, &id).await;
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
    if let Some(catalog) = &state.catalog
        && let Err(error) = catalog.set_memory_state(
            &def.workspace_id,
            &id,
            awaken_protocol_managed::resource_plane::ResourceState::Deleted,
        )
    {
        return err(StatusCode::CONFLICT, error.to_string());
    }
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
    if let Some(catalog) = &state.catalog
        && let Err(error) = catalog.set_memory_state(
            &def.workspace_id,
            &id,
            awaken_protocol_managed::resource_plane::ResourceState::Archived,
        )
    {
        return err(StatusCode::CONFLICT, error.to_string());
    }
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
        id: String::new(),
        memory_id: memory_id.to_string(),
        operation: operation.to_string(),
        content,
        path: path.to_string(),
        redacted_at: None,
    };
    state.versions.append(store, ver);
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
    scope: Option<Extension<WorkspaceScope>>,
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
    let workspace = request_workspace(&state, scope);
    if state.host.memory_get_in(&workspace, &id).await.is_none() {
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
    let log = state.versions.list(&id);
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
    let log = state.versions.list(&id);
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
    if let Some(version) = state.versions.redact(&id, &vid) {
        return (StatusCode::OK, Json(version.project(&id))).into_response();
    }
    not_found("memory_version")
}

#[cfg(test)]
mod version_repository_tests {
    use super::*;

    #[test]
    fn version_history_and_redaction_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let repository = VersionRepository::open_at(dir.path());
        repository.append(
            "memstore_1",
            MemoryVersion {
                id: String::new(),
                memory_id: "memory_1".into(),
                operation: "created".into(),
                content: Some("secret".into()),
                path: "/note".into(),
                redacted_at: None,
            },
        );
        let version_id = repository.list("memstore_1")[0].id.clone();
        repository.redact("memstore_1", &version_id).unwrap();
        drop(repository);

        let reopened = VersionRepository::open_at(dir.path());
        let versions = reopened.list("memstore_1");
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].id, version_id);
        assert_eq!(versions[0].content, None);
        assert!(versions[0].redacted_at.is_some());
    }
}
