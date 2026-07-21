//! The memory-store API (`/v1/memory_stores`) behind the ADR-0038 MemoryStore
//! resource family, aligned to the official `@anthropic-ai/sdk`
//! `beta.memoryStores.*` surface: the store (create / retrieve / update / list /
//! delete / archive), the `memories` subresource (create / retrieve / update /
//! list / delete, with a `content_sha256` precondition), and `memory_versions`
//! (retrieve / list / redact).
//!
//! Each **memory** is a path-addressed file with a `content_sha256` + CAS update in
//! the durable [`awaken_memory_store::MemoryRepository`] (ADR-0053). The same aggregate
//! repository serves API heads, write-through mounts, recall/extraction, and the
//! `/memory_versions` history: every mutation and its version row commit together.
//! There is no API-side history registry or Host-global memory directory. The
//! legacy no-body `POST /v1/memory_stores` still works; the SDK sends a `name`.

use std::sync::Arc;

use awaken_memory_store::{MemErr, MemoryVersion, MemoryVersionOperation};
use awaken_protocol_managed::resource_plane::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, ResourceCatalogError,
    ResourceKind, ResourceState, ResourceTarget,
};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::host::SharedHost;
use crate::resource_scope::RequiredWorkspaceScope;

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

fn project_version(version: &MemoryVersion, store_id: &str) -> Value {
    let (sha, size) = match &version.content {
        Some(content) => (
            Some(awaken_memory_store::sha256_hex(content)),
            Some(content.len()),
        ),
        None => (None, None),
    };
    let operation = match version.operation {
        MemoryVersionOperation::Created => "created",
        MemoryVersionOperation::Modified => "modified",
        MemoryVersionOperation::Deleted => "deleted",
    };
    json!({
        "id": version.id,
        "type": "memory_version",
        "created_at": OBJECT_AT,
        "memory_id": version.memory_id,
        "memory_store_id": store_id,
        "operation": operation,
        "content": version.content,
        "content_sha256": sha,
        "content_size_bytes": size,
        "path": version.path,
        "redacted_at": version.redacted_unix_nanos.map(|_| OBJECT_AT),
    })
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

/// Project a durable [`MemoryStoreDefinition`] (the identity aggregate, source of truth for a
/// store's name/description/metadata) onto the SDK memory-store object. The
/// `created_at`/`updated_at` are stable object constants (the def carries no clock);
/// `archived_at` is stamped from the def's archived flag.
fn project_def(def: &MemoryStoreDefinition) -> Value {
    json!({
        "id": def.id,
        "type": "memory_store",
        "created_at": OBJECT_AT,
        "updated_at": OBJECT_AT,
        "name": def.name,
        "description": def.description,
        "metadata": def.metadata,
        "archived_at": if matches!(def.state, ResourceState::Archived | ResourceState::Deleted) {
            Some(OBJECT_AT)
        } else {
            None
        },
    })
}

fn project_config(config: &MemoryStoreConfigVersion) -> Value {
    json!({
        "memory_store_id": config.memory_store_id,
        "version": config.version.0,
        "recall_policy": config.recall_policy,
        "extraction_policy": config.extraction_policy,
        "retention_policy": config.retention_policy,
    })
}

struct MemoryStoreApi {
    host: Arc<SharedHost>,
    /// Unified resource definition/configuration/lifecycle repository consumed by
    /// both this API and Session resolution. It contains no authorization policy.
    catalog: Arc<dyn awaken_protocol_managed::ResourceCatalog>,
}

/// Mount the memory-store API over an ephemeral resource catalog. Product
/// composition roots must use [`memory_stores_router_with_catalog`] so API and
/// Session resolution share one definition/configuration/lifecycle truth.
pub fn memory_stores_router(host: Arc<SharedHost>) -> Router {
    memory_stores_router_with_catalog(
        host,
        Arc::new(awaken_config_resolver::InMemoryResourceCatalog::new()),
    )
}

/// Mount the Memory API over the same Resource Catalog used by Session
/// resolution. Composition roots that manage resources must use this variant so
/// create/archive/delete and activation share one lifecycle truth.
pub fn memory_stores_router_with_catalog(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_protocol_managed::ResourceCatalog>,
) -> Router {
    let state = Arc::new(MemoryStoreApi { host, catalog });
    Router::new()
        .route("/v1/memory_stores", post(create_store).get(list_stores))
        .route(
            "/v1/memory_stores/{id}",
            get(get_store).post(update_store).delete(delete_store),
        )
        .route(
            "/v1/memory_stores/{id}/config",
            get(get_store_config).post(publish_store_config),
        )
        .route(
            "/v1/memory_stores/{id}/config_versions/{version}",
            get(get_store_config_version),
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

fn catalog_error(error: ResourceCatalogError) -> axum::response::Response {
    let status = match error {
        ResourceCatalogError::AlreadyExists(_) | ResourceCatalogError::ConfigConflict { .. } => {
            StatusCode::CONFLICT
        }
        ResourceCatalogError::NotFound(_) => StatusCode::NOT_FOUND,
        ResourceCatalogError::NotActive { .. } => StatusCode::CONFLICT,
        ResourceCatalogError::Invalid(_) => StatusCode::BAD_REQUEST,
        ResourceCatalogError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    err(status, error.to_string())
}

fn active_store_exists(state: &MemoryStoreApi, workspace: &str, id: &str) -> bool {
    state
        .catalog
        .memory_store(workspace, id)
        .is_some_and(|definition| definition.state == ResourceState::Active)
}

fn mint_memory_store_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("memstore_{nanos:032x}_{sequence:016x}")
}

// ---- Store routes ----------------------------------------------------------

/// `POST /v1/memory_stores` — create a store. The SDK sends `{name, description?,
/// metadata?}`; an empty body keeps the name empty. The Resource Catalog owns
/// identity/existence while MemoryRepository owns only path-addressed content.
async fn create_store(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    body: Bytes,
) -> impl IntoResponse {
    let parsed: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    let id = mint_memory_store_id();
    let def = MemoryStoreDefinition {
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
        state: ResourceState::Active,
        current_config_version: ConfigVersion::INITIAL,
    };
    let projected = project_def(&def);
    if let Err(error) = state.catalog.create_memory_store(
        def,
        MemoryStoreConfigVersion {
            memory_store_id: id,
            version: ConfigVersion::INITIAL,
            recall_policy: Default::default(),
            extraction_policy: Default::default(),
            retention_policy: Default::default(),
        },
    ) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("memory_store catalog write failed: {error}"),
        );
    }
    (StatusCode::OK, Json(projected)).into_response()
}

/// `GET /v1/memory_stores/:id` — the governed store definition. Mutable content
/// is exposed only through `/memories`, the same MemoryRepository used by execution.
async fn get_store(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(def) = state.catalog.memory_store(&workspace, &id) else {
        return not_found("memory_store");
    };
    (StatusCode::OK, Json(project_def(&def))).into_response()
}

async fn list_stores(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
) -> impl IntoResponse {
    let data: Vec<Value> = state
        .catalog
        .list_memory_stores(&workspace)
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
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let Some(mut def) = state.catalog.memory_store(&workspace, &id) else {
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
    if let Err(error) = state.catalog.update_memory_store(def) {
        return err(StatusCode::CONFLICT, error.to_string());
    }
    (StatusCode::OK, Json(projected)).into_response()
}

/// Return the currently selected immutable MemoryStore configuration. Mutable
/// Memory entries are deliberately absent: only recall/extraction/retention
/// behavior is versioned here.
async fn get_store_config(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(definition) = state.catalog.memory_store(&workspace, &id) else {
        return not_found("memory_store");
    };
    let Some(config) =
        state
            .catalog
            .memory_config(&workspace, &id, definition.current_config_version)
    else {
        return err(
            StatusCode::CONFLICT,
            "current MemoryStore config is missing",
        );
    };
    (StatusCode::OK, Json(project_config(&config))).into_response()
}

/// Read an immutable historical configuration by ordinal. This is configuration
/// audit/retry data, not a snapshot of mutable Memory content.
async fn get_store_config_version(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, version)): Path<(String, String)>,
) -> axum::response::Response {
    let Ok(version) = version.parse::<u64>() else {
        return err(StatusCode::BAD_REQUEST, "config version must be an integer");
    };
    let Some(config) = state
        .catalog
        .memory_config(&workspace, &id, ConfigVersion(version))
    else {
        return not_found("memory_store config");
    };
    (StatusCode::OK, Json(project_config(&config))).into_response()
}

/// Publish the next immutable MemoryStore configuration with an explicit CAS
/// fence. Policy values are resource behavior, not authorization policy.
async fn publish_store_config(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let Some(definition) = state.catalog.memory_store(&workspace, &id) else {
        return not_found("memory_store");
    };
    let Some(expected) = body
        .get("expected_config_version")
        .and_then(Value::as_u64)
        .map(ConfigVersion)
    else {
        return err(
            StatusCode::BAD_REQUEST,
            "expected_config_version must be an integer",
        );
    };
    let Some(mut config) =
        state
            .catalog
            .memory_config(&workspace, &id, definition.current_config_version)
    else {
        return err(
            StatusCode::CONFLICT,
            "current MemoryStore config is missing",
        );
    };
    if !["recall_policy", "extraction_policy", "retention_policy"]
        .iter()
        .any(|key| body.get(key).is_some())
    {
        return err(
            StatusCode::BAD_REQUEST,
            "config update has no policy fields",
        );
    }
    if let Some(value) = body.get("recall_policy") {
        config.recall_policy = match serde_json::from_value(value.clone()) {
            Ok(policy) => policy,
            Err(error) => return err(StatusCode::BAD_REQUEST, error.to_string()),
        };
    }
    if let Some(value) = body.get("extraction_policy") {
        config.extraction_policy = match serde_json::from_value(value.clone()) {
            Ok(policy) => policy,
            Err(error) => return err(StatusCode::BAD_REQUEST, error.to_string()),
        };
    }
    if let Some(value) = body.get("retention_policy") {
        config.retention_policy = match serde_json::from_value(value.clone()) {
            Ok(policy) => policy,
            Err(error) => return err(StatusCode::BAD_REQUEST, error.to_string()),
        };
    }
    let Some(next) = expected.checked_next() else {
        return err(StatusCode::BAD_REQUEST, "config version is exhausted");
    };
    config.version = next;
    match state
        .catalog
        .publish_memory_config(&workspace, expected, config.clone())
    {
        Ok(()) => (StatusCode::OK, Json(project_config(&config))).into_response(),
        Err(error) => catalog_error(error),
    }
}

async fn delete_store(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(definition) = state.catalog.memory_store(&workspace, &id) else {
        return not_found("memory_store");
    };
    let Some(config) =
        state
            .catalog
            .memory_config(&workspace, &id, definition.current_config_version)
    else {
        return err(
            StatusCode::CONFLICT,
            "current MemoryStore config is missing",
        );
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    let retention_ms = config
        .retention_policy
        .retention_days
        .map_or(0, |days| u64::from(days).saturating_mul(86_400_000));
    if let Err(error) = state
        .host
        .request_resource_purge(
            ResourceTarget::new(&workspace, ResourceKind::MemoryStore, &id),
            Some(definition.current_config_version.0),
            now,
            now.saturating_add(retention_ms),
        )
        .await
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    if let Err(error) = state
        .catalog
        .set_memory_state(&workspace, &id, ResourceState::Deleted)
    {
        return err(StatusCode::CONFLICT, error.to_string());
    }
    (
        StatusCode::OK,
        Json(json!({ "id": id, "type": "memory_store_deleted" })),
    )
        .into_response()
}

async fn archive_store(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(mut def) = state.catalog.memory_store(&workspace, &id) else {
        return not_found("memory_store");
    };
    if let Err(error) = state
        .catalog
        .set_memory_state(&workspace, &id, ResourceState::Archived)
    {
        return err(StatusCode::CONFLICT, error.to_string());
    }
    def.state = ResourceState::Archived;
    let projected = project_def(&def);
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
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
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
    if !active_store_exists(&state, &workspace, &id) {
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
        Ok(mem) => (StatusCode::OK, Json(project_memory(&mem, &id))).into_response(),
        Err(MemErr::PathConflict(_)) => memory_conflict(),
        Err(MemErr::InvalidPath(_)) => err(StatusCode::BAD_REQUEST, "invalid memory path"),
        Err(MemErr::TooLarge) => err(StatusCode::BAD_REQUEST, "memory content too large"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn list_memories(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    if !active_store_exists(&state, &workspace, &id) {
        return not_found("memory_store");
    }
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
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, mid)): Path<(String, String)>,
) -> axum::response::Response {
    if !active_store_exists(&state, &workspace, &id) {
        return not_found("memory_store");
    }
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
/// swap in the store. Content, optional rename-replace, and history are one atomic
/// aggregate operation and append one `modified` version for the updated memory.
async fn update_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, mid)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if !active_store_exists(&state, &workspace, &id) {
        return not_found("memory_store");
    }
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
    let target_path = body.get("path").and_then(Value::as_str);

    match state
        .host
        .memory_stores
        .fs()
        .update_head(&id, &mid, &new_content, &base_sha, target_path)
        .await
    {
        Ok(updated) => (StatusCode::OK, Json(project_memory(&updated, &id))).into_response(),
        Err(MemErr::Conflict { .. }) => memory_conflict(),
        Err(MemErr::TooLarge) => err(StatusCode::BAD_REQUEST, "memory content too large"),
        Err(MemErr::InvalidPath(_)) => err(StatusCode::BAD_REQUEST, "invalid memory path"),
        Err(MemErr::NotFound(_)) => not_found("memory"),
        Err(error) => err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

async fn delete_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, mid)): Path<(String, String)>,
) -> axum::response::Response {
    if !active_store_exists(&state, &workspace, &id) {
        return not_found("memory_store");
    }
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

/// Whether the resource catalog contains this store in the trusted Workspace.
async fn list_versions(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> axum::response::Response {
    if !active_store_exists(&state, &workspace, &id) {
        return not_found("memory_store");
    }
    let log = match state.host.memory_stores.fs().list_versions(&id).await {
        Ok(log) => log,
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let data: Vec<Value> = collect_versions(&log)
        .iter()
        .map(|version| project_version(version, &id))
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "data": data, "has_more": false, "next_page": null })),
    )
        .into_response()
}

async fn get_version(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, vid)): Path<(String, String)>,
) -> axum::response::Response {
    if !active_store_exists(&state, &workspace, &id) {
        return not_found("memory_store");
    }
    let log = match state.host.memory_stores.fs().list_versions(&id).await {
        Ok(log) => log,
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    match collect_versions(&log).into_iter().find(|v| v.id == vid) {
        Some(version) => (StatusCode::OK, Json(project_version(&version, &id))).into_response(),
        None => not_found("memory_version"),
    }
}

/// `POST /v1/memory_stores/:id/memory_versions/:vid/redact` — redact a version:
/// stamp `redacted_at` and drop its content.
async fn redact_version(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, vid)): Path<(String, String)>,
    _query: Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    if !active_store_exists(&state, &workspace, &id) {
        return not_found("memory_store");
    }
    match state
        .host
        .memory_stores
        .fs()
        .redact_version(&id, &vid)
        .await
    {
        Ok(Some(version)) => {
            return (StatusCode::OK, Json(project_version(&version, &id))).into_response();
        }
        Ok(None) => {}
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
    not_found("memory_version")
}
