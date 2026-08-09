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
use awaken_resource_contract::{
    CreateMemoryStoreCommand, MemoryStoreApplicationError, MemoryStoreApplicationService,
    MemoryStoreDefinition, ResourceCatalogError, ResourceState, UpdateMemoryStoreCommand,
};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::resource_scope::RequiredWorkspaceScope;

fn timestamp(nanos: u128) -> String {
    awaken_session_contract::epoch_millis_to_rfc3339(
        (nanos / 1_000_000).min(u64::MAX as u128) as u64
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemoryView {
    Basic,
    Full,
}

impl MemoryView {
    fn parse(
        query: &std::collections::HashMap<String, String>,
        default: Self,
    ) -> Result<Self, &'static str> {
        match query.get("view").map(String::as_str) {
            None => Ok(default),
            Some("basic") => Ok(Self::Basic),
            Some("full") => Ok(Self::Full),
            Some(_) => Err("view must be `basic` or `full`"),
        }
    }

    fn includes_content(self) -> bool {
        self == Self::Full
    }
}

fn project_version(version: &MemoryVersion, store_id: &str, view: MemoryView) -> Value {
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
        "created_at": timestamp(version.created_unix_nanos),
        "memory_id": version.memory_id,
        "memory_store_id": store_id,
        "operation": operation,
        "content": view.includes_content().then(|| version.content.clone()).flatten(),
        "content_sha256": sha,
        "content_size_bytes": size,
        "path": version.path,
        "redacted_at": version.redacted_unix_nanos.map(timestamp),
    })
}

/// Project a durable [`awaken_memory_store::Memory`] (the path-addressed head, the
/// source of truth) onto the SDK memory object.
fn project_memory(
    mem: &awaken_memory_store::Memory,
    store_id: &str,
    memory_version_id: &str,
    view: MemoryView,
) -> Value {
    let content = view
        .includes_content()
        .then(|| mem.content.clone().unwrap_or_default());
    json!({
        "id": mem.id,
        "type": "memory",
        "created_at": timestamp(mem.created_unix_nanos),
        "updated_at": timestamp(mem.updated_unix_nanos),
        "memory_store_id": store_id,
        "memory_version_id": memory_version_id,
        "path": mem.path,
        "content": content,
        "content_sha256": mem.content_sha256,
        "content_size_bytes": mem.content_size,
    })
}

fn current_version_id<'a>(versions: &'a [MemoryVersion], memory_id: &str) -> Option<&'a str> {
    versions
        .iter()
        .rev()
        .find(|version| version.memory_id == memory_id)
        .map(|version| version.id.as_str())
}

async fn project_current_memory(
    state: &MemoryStoreApi,
    memory: &awaken_memory_store::Memory,
    store_id: &str,
    view: MemoryView,
) -> Result<Value, MemErr> {
    let versions = state.memories.list_versions(store_id).await?;
    let version_id = current_version_id(&versions, &memory.id).ok_or_else(|| {
        MemErr::Storage(format!(
            "memory `{}` has no atomic current version",
            memory.id
        ))
    })?;
    Ok(project_memory(memory, store_id, version_id, view))
}

/// Project a durable [`MemoryStoreDefinition`] (the identity aggregate, source of truth for a
/// store's name/description/metadata) onto the SDK memory-store object. The
fn project_def(def: &MemoryStoreDefinition) -> Value {
    json!({
        "id": def.id,
        "type": "memory_store",
        "created_at": timestamp(def.timestamps.created_unix_nanos.into()),
        "updated_at": timestamp(def.timestamps.updated_unix_nanos.into()),
        "name": def.name,
        "description": def.description,
        "metadata": def.metadata,
        "archived_at": if matches!(def.state, ResourceState::Archived | ResourceState::Deleted) {
            def.timestamps.archived_unix_nanos.map(|value| timestamp(value.into()))
        } else {
            None
        },
    })
}

struct MemoryStoreApi {
    memories: Arc<dyn awaken_memory_store::MemoryRepository>,
    stores: Arc<dyn MemoryStoreApplicationService>,
}

/// Mount the Memory API over the same Resource Catalog used by Session
/// resolution. Composition roots that manage resources must use this variant so
/// create/archive/delete and activation share one lifecycle truth.
pub fn memory_stores_router(
    memories: Arc<dyn awaken_memory_store::MemoryRepository>,
    stores: Arc<dyn MemoryStoreApplicationService>,
) -> Router {
    let state = Arc::new(MemoryStoreApi { memories, stores });
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

fn catalog_error(error: ResourceCatalogError) -> axum::response::Response {
    let status = match error {
        ResourceCatalogError::AlreadyExists(_) | ResourceCatalogError::ConfigConflict { .. } => {
            StatusCode::CONFLICT
        }
        ResourceCatalogError::NotFound(_) | ResourceCatalogError::ConfigNotFound { .. } => {
            StatusCode::NOT_FOUND
        }
        ResourceCatalogError::NotActive { .. } => StatusCode::CONFLICT,
        ResourceCatalogError::Invalid(_) => StatusCode::BAD_REQUEST,
        ResourceCatalogError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    err(status, error.to_string())
}

fn application_error(error: MemoryStoreApplicationError) -> axum::response::Response {
    match error {
        MemoryStoreApplicationError::Catalog(error) => catalog_error(error),
        MemoryStoreApplicationError::Purge(error) => {
            err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    }
}

async fn active_store_exists(
    state: &MemoryStoreApi,
    workspace: &str,
    id: &str,
) -> Result<bool, MemoryStoreApplicationError> {
    state.stores.get(workspace, id).await.map(|definition| {
        definition.is_some_and(|definition| definition.state == ResourceState::Active)
    })
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
    let definition = match state
        .stores
        .create(CreateMemoryStoreCommand {
            workspace_id: workspace,
            id: None,
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
            initial_state: ResourceState::Active,
            retention_policy: Default::default(),
        })
        .await
    {
        Ok(definition) => definition,
        Err(error) => return application_error(error),
    };
    (StatusCode::OK, Json(project_def(&definition))).into_response()
}

/// `GET /v1/memory_stores/:id` — the governed store definition. Mutable content
/// is exposed only through `/memories`, the same MemoryRepository used by execution.
async fn get_store(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> axum::response::Response {
    let def = match state.stores.get(&workspace, &id).await {
        Ok(Some(definition)) => definition,
        Ok(None) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    };
    (StatusCode::OK, Json(project_def(&def))).into_response()
}

async fn list_stores(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
) -> axum::response::Response {
    let definitions = match state.stores.list(&workspace).await {
        Ok(definitions) => definitions,
        Err(error) => return application_error(error),
    };
    let data: Vec<Value> = definitions.iter().map(project_def).collect();
    (
        StatusCode::OK,
        Json(json!({ "data": data, "has_more": false, "next_page": null })),
    )
        .into_response()
}

async fn update_store(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let metadata_patch = body
        .get("metadata")
        .and_then(Value::as_object)
        .map(|patch| {
            patch
                .iter()
                .filter_map(|(key, value)| match value {
                    Value::Null => Some((key.clone(), None)),
                    Value::String(value) => Some((key.clone(), Some(value.clone()))),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    match state
        .stores
        .update(UpdateMemoryStoreCommand {
            workspace_id: workspace,
            id: id.into(),
            description: body
                .get("description")
                .map(|value| value.as_str().unwrap_or_default().to_string()),
            metadata_patch,
        })
        .await
    {
        Ok(definition) => (StatusCode::OK, Json(project_def(&definition))).into_response(),
        Err(error) => application_error(error),
    }
}

async fn delete_store(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> axum::response::Response {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    if let Err(error) = state.stores.delete(&workspace, &id, now).await {
        return application_error(error);
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
    match state
        .stores
        .set_state(&workspace, &id, ResourceState::Archived)
        .await
    {
        Ok(definition) => (StatusCode::OK, Json(project_def(&definition))).into_response(),
        Err(error) => application_error(error),
    }
}

// ---- Memory routes ---------------------------------------------------------

/// Find a memory's current path by its id in the durable store (path-addressed, so
/// an id lookup scans the store's listing — memory stores are small).
async fn path_of(state: &MemoryStoreApi, store: &str, mid: &str) -> Option<String> {
    state
        .memories
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
    Query(query): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let view = match MemoryView::parse(&query, MemoryView::Basic) {
        Ok(view) => view,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let Some(path) = body.get("path").and_then(Value::as_str) else {
        return err(StatusCode::BAD_REQUEST, "memory needs a `path`");
    };
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    // The durable path-addressed store is the source of truth for the head.
    match state.memories.create(&id, path, content).await {
        Ok(mem) => match project_current_memory(&state, &mem, &id, view).await {
            Ok(projected) => (StatusCode::OK, Json(projected)).into_response(),
            Err(error) => err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(MemErr::PathConflict(_)) => memory_conflict(),
        Err(MemErr::InvalidPath(_)) => err(StatusCode::BAD_REQUEST, "invalid memory path"),
        Err(MemErr::TooLarge) => err(StatusCode::BAD_REQUEST, "memory content too large"),
        Err(error @ MemErr::AtCapacity) => err(StatusCode::BAD_REQUEST, error.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn list_memories(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    let prefix = q.get("path_prefix").map(String::as_str).unwrap_or("/");
    if !prefix.starts_with('/') || !prefix.ends_with('/') {
        return err(
            StatusCode::BAD_REQUEST,
            "path_prefix must be absolute and end with `/`",
        );
    }
    let depth = match q.get("depth").map(String::as_str) {
        None | Some("0") => 0,
        Some("1") => 1,
        Some(_) => return err(StatusCode::BAD_REQUEST, "depth must be 0 or 1"),
    };
    let view = match MemoryView::parse(&q, MemoryView::Basic) {
        Ok(view) => view,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let requested_limit = match q.get("limit") {
        None => awaken_agent_contract::page::DEFAULT_PAGE_LIMIT,
        Some(value) => match value.parse::<usize>() {
            Ok(0) | Err(_) => return err(StatusCode::BAD_REQUEST, "limit must be positive"),
            Ok(value) => value.min(awaken_agent_contract::page::MAX_PAGE_LIMIT),
        },
    };
    let limit = if view == MemoryView::Basic {
        requested_limit
    } else {
        requested_limit.min(20)
    };
    let entries = match state.memories.list(&id, prefix).await {
        Ok(entries) => entries,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let versions = match state.memories.list_versions(&id).await {
        Ok(versions) => versions,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let mut data = Vec::with_capacity(entries.len());
    let mut rolled_up = std::collections::BTreeSet::new();
    for entry in entries {
        if prefix != "/" && !entry.path.starts_with(prefix) {
            continue;
        }
        let relative = if prefix == "/" {
            entry.path.trim_start_matches('/')
        } else {
            entry.path.strip_prefix(prefix).unwrap_or_default()
        };
        if depth == 1
            && let Some((directory, _)) = relative.split_once('/')
        {
            rolled_up.insert(format!("{prefix}{directory}/"));
            continue;
        }
        // `basic` elides content (metadata only); `full` fetches the head content.
        let content = if view == MemoryView::Basic {
            None
        } else {
            state
                .memories
                .get_by_path(&id, &entry.path)
                .await
                .ok()
                .flatten()
                .and_then(|m| m.content)
        };
        let Some(memory_version_id) = current_version_id(&versions, &entry.id) else {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("memory `{}` has no atomic current version", entry.id),
            );
        };
        data.push(json!({
            "id": entry.id,
            "type": "memory",
            "created_at": timestamp(entry.updated_unix_nanos),
            "updated_at": timestamp(entry.updated_unix_nanos),
            "memory_store_id": id,
            "memory_version_id": memory_version_id,
            "path": entry.path,
            "content": content,
            "content_sha256": entry.content_sha256,
            "content_size_bytes": entry.content_size,
        }));
    }
    data.extend(
        rolled_up
            .into_iter()
            .map(|path| json!({ "type": "memory_prefix", "path": path })),
    );
    data.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    let page = match awaken_agent_contract::page::paginate_by_key(
        &data,
        q.get("page").map(String::as_str),
        Some(limit),
        |item| item["path"].as_str().unwrap_or_default().to_string(),
    ) {
        Ok(page) => page,
        Err(_) => return err(StatusCode::BAD_REQUEST, "invalid memory pagination cursor"),
    };
    (
        StatusCode::OK,
        Json(json!({
            "data": page.items,
            "has_more": page.has_more,
            "next_page": page.next_page,
        })),
    )
        .into_response()
}

async fn get_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, mid)): Path<(String, String)>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let view = match MemoryView::parse(&query, MemoryView::Full) {
        Ok(view) => view,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    let Some(path) = path_of(&state, &id, &mid).await else {
        return not_found("memory");
    };
    match state.memories.get_by_path(&id, &path).await {
        Ok(Some(mem)) => match project_current_memory(&state, &mem, &id, view).await {
            Ok(projected) => (StatusCode::OK, Json(projected)).into_response(),
            Err(error) => err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
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
    Query(query): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let view = match MemoryView::parse(&query, MemoryView::Basic) {
        Ok(view) => view,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    let Some(path) = path_of(&state, &id, &mid).await else {
        return not_found("memory");
    };
    let Ok(Some(current)) = state.memories.get_by_path(&id, &path).await else {
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
        .memories
        .update_head(&id, &mid, &new_content, &base_sha, target_path)
        .await
    {
        Ok(updated) => match project_current_memory(&state, &updated, &id, view).await {
            Ok(projected) => (StatusCode::OK, Json(projected)).into_response(),
            Err(error) => err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(MemErr::Conflict { .. }) => memory_conflict(),
        Err(MemErr::TooLarge) => err(StatusCode::BAD_REQUEST, "memory content too large"),
        Err(error @ MemErr::AtCapacity) => err(StatusCode::BAD_REQUEST, error.to_string()),
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
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    let Some(path) = path_of(&state, &id, &mid).await else {
        return not_found("memory");
    };
    if state.memories.delete_by_path(&id, &path).await.is_err() {
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
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let view = match MemoryView::parse(&query, MemoryView::Basic) {
        Ok(view) => view,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    let log = match state.memories.list_versions(&id).await {
        Ok(log) => log,
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let data: Vec<Value> = collect_versions(&log)
        .iter()
        .map(|version| project_version(version, &id, view))
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
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let view = match MemoryView::parse(&query, MemoryView::Full) {
        Ok(view) => view,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    let log = match state.memories.list_versions(&id).await {
        Ok(log) => log,
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    match collect_versions(&log).into_iter().find(|v| v.id == vid) {
        Some(version) => {
            (StatusCode::OK, Json(project_version(&version, &id, view))).into_response()
        }
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
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    match state.memories.redact_version(&id, &vid).await {
        Ok(Some(version)) => {
            return (
                StatusCode::OK,
                Json(project_version(&version, &id, MemoryView::Full)),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
    not_found("memory_version")
}
