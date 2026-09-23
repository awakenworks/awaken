//! The memory-store API (`/v1/memory_stores`) behind the ADR-0038 MemoryStore
//! resource family, aligned to the official `@anthropic-ai/sdk`
//! `beta.memoryStores.*` surface: the store (create / retrieve / update / list /
//! delete / archive), the `memories` subresource (create / retrieve / update /
//! list / delete, with a `content_sha256` precondition), and `memory_versions`
//! (retrieve / list / redact).
//!
//! Each **memory** is a path-addressed file with a `content_sha256` + CAS update in
//! the durable [`awaken_resource_contract::MemoryRepository`] (ADR-0053). The same aggregate
//! repository serves API heads, write-through mounts, recall/extraction, and the
//! `/memory_versions` history: every mutation and its version row commit together.
//! There is no API-side history registry or Host-global memory directory. The
//! legacy no-body `POST /v1/memory_stores` still works; the SDK sends a `name`.

use std::sync::Arc;

use awaken_resource_contract::{
    CreateMemoryStoreCommand, MemErr, Memory, MemoryActor as DomainMemoryActor, MemoryRepository,
    MemoryStoreApplicationError, MemoryStoreApplicationService, MemoryStoreDefinition,
    MemoryVersion, MemoryVersionOperation, ResourceRegistryError, ResourceState,
    UpdateMemoryStoreCommand, memory_sha256_hex,
};
use axum::Extension;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::common::headers::parse_idempotency_key_header;
use crate::common::scope::RequiredWorkspaceScope;
use crate::routes::{ManagedJson, ManagedQuery};
use crate::types::memory::{
    AuthenticatedMemoryActor, MemoryActor, MemoryVersion as MemoryVersionObject,
    MemoryVersionOperation as MemoryVersionOperationObject,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate, paginate_by};

use super::managed_resource_error as err;
mod copy_conflicts;

fn timestamp(nanos: u128) -> String {
    awaken_session_application_contract::epoch_millis_to_rfc3339(
        awaken_session_application_contract::project_epoch_millis(nanos / 1_000_000),
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

fn optional_query<'a>(
    query: &'a std::collections::HashMap<String, String>,
    name: &str,
) -> Option<&'a str> {
    query
        .get(name)
        .and_then(|value| crate::types::page::non_empty_query_value(value))
}

fn memory_path_prefix(query: &std::collections::HashMap<String, String>) -> &str {
    optional_query(query, "path_prefix").unwrap_or("/")
}

fn parse_page_query(
    query: &std::collections::HashMap<String, String>,
) -> Result<PageQuery, &'static str> {
    let limit = match query.get("limit") {
        None => None,
        Some(value) => match value.parse::<usize>() {
            Ok(0) | Err(_) => return Err("limit must be positive"),
            Ok(value) => Some(value.min(awaken_agent_contract::page::MAX_PAGE_LIMIT)),
        },
    };
    Ok(PageQuery {
        limit,
        page: optional_query(query, "page").map(str::to_owned),
    })
}

#[derive(Clone, Debug, Serialize)]
struct MemoryObject {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    created_at: String,
    updated_at: String,
    memory_store_id: String,
    memory_version_id: String,
    path: String,
    content: Option<String>,
    content_sha256: String,
    content_size_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
struct MemoryPrefix {
    #[serde(rename = "type")]
    kind: &'static str,
    path: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
// This wire-only enum serializes immediately; boxing every memory item would
// add a heap allocation solely to equalize it with the two-string prefix DTO.
#[allow(clippy::large_enum_variant)]
enum MemoryListItem {
    Memory(MemoryObject),
    Prefix(MemoryPrefix),
}

impl MemoryListItem {
    fn path(&self) -> &str {
        match self {
            Self::Memory(memory) => &memory.path,
            Self::Prefix(prefix) => &prefix.path,
        }
    }
}

#[derive(Debug, Serialize)]
struct MemoryStoreObject<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    created_at: String,
    updated_at: String,
    name: &'a str,
    description: &'a str,
    metadata: &'a std::collections::BTreeMap<String, String>,
    archived_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeletedMemoryResource {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemoryStoreCreateParams {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    metadata: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemoryStoreUpdateParams {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    metadata: Option<std::collections::BTreeMap<String, Option<String>>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemoryStoreListParams {
    #[serde(default, rename = "beta")]
    _beta_selector: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    limit: Option<usize>,
    #[serde(
        default,
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    page: Option<String>,
    #[serde(
        default,
        rename = "created_at[gte]",
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    created_at_gte: Option<String>,
    #[serde(
        default,
        rename = "created_at[lte]",
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    created_at_lte: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    include_archived: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemoryDeleteParams {
    #[serde(default, rename = "beta")]
    _beta_selector: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    pub(crate) expected_content_sha256: Option<String>,
}

impl MemoryStoreListParams {
    fn page_query(&self) -> PageQuery {
        PageQuery {
            limit: self.limit,
            page: self.page.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct NullableString(Option<String>);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemoryCreateParams {
    content: NullableString,
    path: String,
}

/// The official optimistic-concurrency precondition
/// (`BetaManagedAgentsPrecondition`): the update applies only if the stored
/// `content_sha256` equals the supplied value. The wire never carries the
/// store's complete generation -- that is an internal CAS witness -- so the
/// handler binds the supplied hash to the head it loaded and hands the store
/// that head's complete generation. Replacing this shape with the internal one
/// made every official SDK precondition a 400.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MemoryPrecondition {
    ContentSha256 { content_sha256: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreconditionRefusal {
    /// Not 64 lowercase hexadecimal characters: never a hash this server minted.
    Malformed,
    /// A well-formed hash that is not the head's: the caller read stale state.
    Stale,
}

impl MemoryPrecondition {
    /// Bind the supplied hash to the loaded head. Cause/effect: C1 the hash is
    /// well formed; C2 it equals the head's `content_sha256`. Effects: E1 the
    /// head's complete generation becomes the CAS base; E2 `Stale`; E3
    /// `Malformed`. Rules: C1+C2 -> E1; C1+!C2 -> E2; !C1 -> E3.
    fn bind(
        &self,
        current: &awaken_resource_contract::Memory,
    ) -> Result<awaken_resource_contract::MemoryGeneration, PreconditionRefusal> {
        let Self::ContentSha256 { content_sha256 } = self;
        if content_sha256.len() != 64
            || !content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(PreconditionRefusal::Malformed);
        }
        if *content_sha256 != current.content_sha256 {
            return Err(PreconditionRefusal::Stale);
        }
        Ok(current.generation())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemoryUpdateParams {
    #[serde(default, deserialize_with = "crate::types::presence::double_option")]
    content: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::types::presence::double_option")]
    path: Option<Option<String>>,
    #[serde(default)]
    pub(crate) precondition: Option<MemoryPrecondition>,
}

fn project_version(
    version: &MemoryVersion,
    store_id: &str,
    view: MemoryView,
) -> MemoryVersionObject {
    let (sha, size) = match &version.content {
        Some(content) => (Some(memory_sha256_hex(content)), Some(content.len())),
        None => (None, None),
    };
    let operation = match version.operation {
        MemoryVersionOperation::Created => MemoryVersionOperationObject::Created,
        MemoryVersionOperation::Modified => MemoryVersionOperationObject::Modified,
        MemoryVersionOperation::Deleted => MemoryVersionOperationObject::Deleted,
    };
    let actor = |actor: &DomainMemoryActor| match actor {
        DomainMemoryActor::ApiActor { api_key_id } => MemoryActor::ApiActor {
            api_key_id: api_key_id.clone(),
        },
        DomainMemoryActor::SessionActor { session_id } => MemoryActor::SessionActor {
            session_id: session_id.clone(),
        },
        DomainMemoryActor::UserActor { user_id } => MemoryActor::UserActor {
            user_id: user_id.clone(),
        },
        DomainMemoryActor::ServiceAccountActor { service_account_id } => {
            MemoryActor::ServiceAccountActor {
                service_account_id: service_account_id.clone(),
            }
        }
    };
    MemoryVersionObject {
        id: version.id.clone(),
        kind: crate::types::memory::MemoryVersionObjectType::MemoryVersion,
        created_at: timestamp(version.created_unix_nanos),
        memory_id: version.memory_id.clone(),
        memory_store_id: store_id.to_string(),
        operation,
        content: view
            .includes_content()
            .then(|| version.content.clone())
            .flatten(),
        content_sha256: sha,
        content_size_bytes: size.map(|size| size as u64),
        created_by: version.created_by.as_ref().map(actor),
        path: Some(version.path.clone()),
        redacted_at: version.redacted_unix_nanos.map(timestamp),
        redacted_by: version.redacted_by.as_ref().map(actor),
    }
}

/// Project a durable [`Memory`] (the path-addressed head, the
/// source of truth) onto the SDK memory object.
fn project_memory(
    mem: &Memory,
    store_id: &str,
    memory_version_id: &str,
    view: MemoryView,
) -> MemoryObject {
    let content = view
        .includes_content()
        .then(|| mem.content.clone().unwrap_or_default());
    MemoryObject {
        id: mem.id.clone(),
        kind: "memory",
        created_at: timestamp(mem.created_unix_nanos),
        updated_at: timestamp(mem.updated_unix_nanos),
        memory_store_id: store_id.to_string(),
        memory_version_id: memory_version_id.to_string(),
        path: mem.path.clone(),
        content,
        content_sha256: mem.content_sha256.clone(),
        content_size_bytes: mem.content_size,
    }
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
    memory: &Memory,
    store_id: &str,
    view: MemoryView,
) -> Result<MemoryObject, MemErr> {
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
fn project_def(def: &MemoryStoreDefinition) -> MemoryStoreObject<'_> {
    MemoryStoreObject {
        id: def.id.as_str(),
        kind: "memory_store",
        created_at: timestamp(def.timestamps.created_unix_nanos.into()),
        updated_at: timestamp(def.timestamps.updated_unix_nanos.into()),
        name: &def.name,
        description: &def.description,
        metadata: &def.metadata,
        archived_at: if matches!(def.state, ResourceState::Archived | ResourceState::Deleted) {
            def.timestamps
                .archived_unix_nanos
                .map(|value| timestamp(value.into()))
        } else {
            None
        },
    }
}

struct MemoryStoreApi {
    memories: Arc<dyn MemoryRepository>,
    stores: Arc<dyn MemoryStoreApplicationService>,
}

/// Mount the Memory API over the same Resource Registry used by Session
/// resolution. Processes that manage Resources must use this variant so
/// create/archive/delete and activation share one lifecycle truth.
pub fn memory_stores_router(
    memories: Arc<dyn MemoryRepository>,
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
            // Published SDKs historically issue POST while the canonical
            // Managed Agents endpoint uses PATCH. Both verbs enter the same
            // aggregate command so CAS and version-history semantics cannot
            // drift between clients.
            get(get_memory)
                .post(update_memory)
                .patch(update_memory)
                .delete(delete_memory),
        )
        .route("/v1/memory_stores/{id}/memory_versions", get(list_versions))
        .merge(awaken_protocol_awaken::memory_copy_conflicts_router(
            copy_conflicts::list,
        ))
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

fn not_found(what: &str) -> axum::response::Response {
    err(StatusCode::NOT_FOUND, format!("{what} not found"))
}

fn registry_error(error: ResourceRegistryError) -> axum::response::Response {
    let status = match error {
        ResourceRegistryError::AlreadyRegistered(_)
        | ResourceRegistryError::ConfigConflict { .. }
        | ResourceRegistryError::ConcurrentModification(_) => StatusCode::CONFLICT,
        ResourceRegistryError::NotFound(_) | ResourceRegistryError::ConfigNotFound { .. } => {
            StatusCode::NOT_FOUND
        }
        ResourceRegistryError::NotActive { .. } => StatusCode::CONFLICT,
        ResourceRegistryError::IdentityRejected { .. }
        | ResourceRegistryError::ConfigurationRejected { .. }
        | ResourceRegistryError::AdmissionRejected { .. } => StatusCode::BAD_REQUEST,
        ResourceRegistryError::VersionExhausted { .. } => StatusCode::CONFLICT,
        ResourceRegistryError::EncodingFailure { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        ResourceRegistryError::CorruptData(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ResourceRegistryError::AuthorityUnavailable { .. }
        | ResourceRegistryError::Indeterminate { .. } => StatusCode::SERVICE_UNAVAILABLE,
    };
    err(status, error.to_string())
}

fn application_error(error: MemoryStoreApplicationError) -> axum::response::Response {
    match error {
        MemoryStoreApplicationError::Registry(error) => registry_error(error),
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

/// `POST /v1/memory_stores` — create a store. The Resource Registry owns
/// identity/existence while MemoryRepository owns only path-addressed content.
async fn create_store(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    headers: HeaderMap,
    ManagedJson(body): ManagedJson<MemoryStoreCreateParams>,
) -> impl IntoResponse {
    let request_key = match parse_idempotency_key_header(&headers) {
        Ok(key) => key,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let command = CreateMemoryStoreCommand {
        workspace_id: workspace,
        id: None,
        name: body.name,
        description: body.description.unwrap_or_default(),
        metadata: body.metadata,
        initial_state: ResourceState::Active,
        retention_policy: Default::default(),
    };
    let result = match request_key {
        Some(key) => state.stores.create_idempotent(command, &key).await,
        None => state.stores.create(command).await,
    };
    let definition = match result {
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
    ManagedQuery(query): ManagedQuery<MemoryStoreListParams>,
) -> axum::response::Response {
    let created_at_gte = match query
        .created_at_gte
        .as_deref()
        .map(|value| parse_version_time(value, "created_at[gte]"))
        .transpose()
    {
        Ok(value) => value,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let created_at_lte = match query
        .created_at_lte
        .as_deref()
        .map(|value| parse_version_time(value, "created_at[lte]"))
        .transpose()
    {
        Ok(value) => value,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let definitions = match state.stores.list(&workspace).await {
        Ok(definitions) => definitions,
        Err(error) => return application_error(error),
    };
    let definitions = definitions
        .into_iter()
        .filter(|definition| {
            let state_matches = match definition.state {
                ResourceState::Deleted => false,
                ResourceState::Archived => query.include_archived.unwrap_or(false),
                ResourceState::Active | ResourceState::Suspended => true,
            };
            let created_at = projected_time(definition.timestamps.created_unix_nanos.into());
            state_matches
                && created_at_gte.is_none_or(|lower| created_at >= lower)
                && created_at_lte.is_none_or(|upper| created_at <= upper)
        })
        .collect();
    let page = paginate(definitions, &query.page_query(), |definition| {
        definition.id.as_str()
    });
    let data: Vec<_> = page.data.iter().map(project_def).collect();
    (
        StatusCode::OK,
        Json(PageCursor {
            data,
            next_page: page.next_page,
        }),
    )
        .into_response()
}

async fn update_store(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<MemoryStoreUpdateParams>,
) -> axum::response::Response {
    match state
        .stores
        .update(UpdateMemoryStoreCommand {
            workspace_id: workspace,
            id: id.into(),
            name: body.name,
            description: body.description,
            metadata_patch: body.metadata.unwrap_or_default(),
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
        Json(DeletedMemoryResource {
            id,
            kind: "memory_store_deleted",
        }),
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

/// Resolve one current head by id from the repository's atomic snapshot.
///
/// Returning the repository error is intentional: storage unavailability must
/// never be projected as a stable public 404, and callers that need content must
/// not reconstruct a mixed-generation head with `list` followed by `get`.
async fn memory_by_id(
    state: &MemoryStoreApi,
    store: &str,
    mid: &str,
) -> Result<Option<Memory>, MemErr> {
    let memory = state
        .memories
        .snapshot_heads(store)
        .await?
        .into_iter()
        .find(|memory| memory.id == mid);
    Ok(memory)
}

fn memory_conflict() -> axum::response::Response {
    (
        StatusCode::CONFLICT,
        Json(ErrorResponse::new(
            "memory_precondition_failed_error",
            "content_sha256 precondition did not match",
        )),
    )
        .into_response()
}

async fn create_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    ManagedQuery(query): ManagedQuery<std::collections::HashMap<String, String>>,
    actor: Option<Extension<AuthenticatedMemoryActor>>,
    ManagedJson(body): ManagedJson<MemoryCreateParams>,
) -> axum::response::Response {
    let view = match MemoryView::parse(&query, MemoryView::Basic) {
        Ok(view) => view,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let content = body.content.0.unwrap_or_default();
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    // The durable path-addressed store is the source of truth for the head.
    match state
        .memories
        .create_as(
            &id,
            &body.path,
            &content,
            actor.as_ref().map(|actor| &actor.0.0),
        )
        .await
    {
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
    ManagedQuery(q): ManagedQuery<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    let prefix = memory_path_prefix(&q);
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
    // Full and basic views share one frozen head snapshot. Besides avoiding an
    // N+1 read, this keeps created_at/content/hash/version from different
    // generations out of one SDK page under concurrent writes.
    let entries = match state.memories.snapshot_heads(&id).await {
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
        let Some(memory_version_id) = current_version_id(&versions, &entry.id) else {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("memory `{}` has no atomic current version", entry.id),
            );
        };
        data.push(MemoryListItem::Memory(project_memory(
            &entry,
            &id,
            memory_version_id,
            view,
        )));
    }
    data.extend(rolled_up.into_iter().map(|path| {
        MemoryListItem::Prefix(MemoryPrefix {
            kind: "memory_prefix",
            path,
        })
    }));
    data.sort_by(|left, right| left.path().cmp(right.path()));
    let page = match awaken_agent_contract::page::paginate_by_key(
        &data,
        optional_query(&q, "page"),
        Some(limit),
        |item| item.path().to_string(),
    ) {
        Ok(page) => page,
        Err(_) => return err(StatusCode::BAD_REQUEST, "invalid memory pagination cursor"),
    };
    (
        StatusCode::OK,
        Json(PageCursor {
            data: page.items.to_vec(),
            next_page: page.next_page,
        }),
    )
        .into_response()
}

async fn get_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, mid)): Path<(String, String)>,
    ManagedQuery(query): ManagedQuery<std::collections::HashMap<String, String>>,
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
    let memory = match memory_by_id(&state, &id, &mid).await {
        Ok(Some(memory)) => memory,
        Ok(None) | Err(MemErr::NotFound(_)) => return not_found("memory"),
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    match project_current_memory(&state, &memory, &id, view).await {
        Ok(projected) => (StatusCode::OK, Json(projected)).into_response(),
        Err(error) => err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// `POST|PATCH /v1/memory_stores/:id/memories/:mid` — update a memory's content (and/or
/// path). A `content_sha256` precondition that does not match the durable head is a
/// `409` (the SDK's `memory_precondition_failed_error`), enforced as a compare-and-
/// swap in the store. Content, optional rename-replace, and history are one atomic
/// aggregate operation and append one `modified` version for the updated memory.
async fn update_memory(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, mid)): Path<(String, String)>,
    ManagedQuery(query): ManagedQuery<std::collections::HashMap<String, String>>,
    actor: Option<Extension<AuthenticatedMemoryActor>>,
    ManagedJson(body): ManagedJson<MemoryUpdateParams>,
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
    let current = match memory_by_id(&state, &id, &mid).await {
        Ok(Some(memory)) => memory,
        Ok(None) | Err(MemErr::NotFound(_)) => return not_found("memory"),
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let new_content = body
        .content
        .flatten()
        .unwrap_or_else(|| current.content.clone().unwrap_or_default());
    let target_path = body.path.flatten();
    // The CAS base is always the complete generation of the head this request
    // loaded. A precondition decides whether that head is the one the caller
    // read: a stale hash conflicts -- except when the stored state already
    // equals the requested content and path, which the SDK contract answers
    // with 200 -- and a malformed hash is refused before any write.
    let base = match body.precondition.as_ref().map(|p| p.bind(&current)) {
        None | Some(Ok(_)) => current.generation(),
        Some(Err(PreconditionRefusal::Malformed)) => {
            return err(
                StatusCode::BAD_REQUEST,
                "precondition.content_sha256 must be 64 lowercase hexadecimal characters",
            );
        }
        Some(Err(PreconditionRefusal::Stale)) => {
            let same_content = current.content.as_deref().unwrap_or_default() == new_content;
            let same_path = target_path
                .as_deref()
                .is_none_or(|path| path == current.path);
            if same_content && same_path {
                return match project_current_memory(&state, &current, &id, view).await {
                    Ok(projected) => (StatusCode::OK, Json(projected)).into_response(),
                    Err(error) => err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
                };
            }
            return memory_conflict();
        }
    };

    match state
        .memories
        .update_head_as(
            &id,
            &base,
            &new_content,
            target_path.as_deref(),
            actor.as_ref().map(|actor| &actor.0.0),
        )
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
    ManagedQuery(query): ManagedQuery<MemoryDeleteParams>,
    actor: Option<Extension<AuthenticatedMemoryActor>>,
) -> axum::response::Response {
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    let current = match memory_by_id(&state, &id, &mid).await {
        Ok(Some(memory)) => memory,
        Ok(None) | Err(MemErr::NotFound(_)) => return not_found("memory"),
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let actor = actor.as_ref().map(|actor| &actor.0.0);
    let path = current.path.clone();
    // `expected_content_sha256` is the official conditional-delete query
    // (`MemoryDeleteParams`); like the update precondition it binds to the
    // head this request loaded, so the store still fences on that head's
    // complete generation without the wire ever carrying a version.
    if let Some(content_sha256) = query.expected_content_sha256.clone() {
        let base = match (MemoryPrecondition::ContentSha256 { content_sha256 }).bind(&current) {
            Ok(generation) => generation,
            Err(PreconditionRefusal::Malformed) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "expected_content_sha256 must be 64 lowercase hexadecimal characters",
                );
            }
            Err(PreconditionRefusal::Stale) => return memory_conflict(),
        };
        match state
            .memories
            .delete_if_match_as(&id, &path, &base, actor)
            .await
        {
            Ok(true) => {}
            Ok(false) | Err(MemErr::NotFound(_)) => return not_found("memory"),
            Err(MemErr::Conflict { .. }) => return memory_conflict(),
            Err(error) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("delete failed: {error}"),
                );
            }
        }
    } else if let Err(error) = state.memories.delete_by_path_as(&id, &path, actor).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("delete failed: {error}"),
        );
    }
    (
        StatusCode::OK,
        Json(DeletedMemoryResource {
            id: mid,
            kind: "memory_deleted",
        }),
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

fn version_operation_name(operation: MemoryVersionOperation) -> &'static str {
    match operation {
        MemoryVersionOperation::Created => "created",
        MemoryVersionOperation::Modified => "modified",
        MemoryVersionOperation::Deleted => "deleted",
    }
}

fn parse_version_time(value: &str, field: &str) -> Result<i128, String> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| {
            i128::from(time.timestamp()) * 1_000_000_000 + i128::from(time.timestamp_subsec_nanos())
        })
        .map_err(|_| format!("{field} must be an RFC 3339 timestamp"))
}

fn projected_time(nanos: u128) -> i128 {
    // The public formatter and filter share one numeric projection authority.
    // Compare in nanoseconds so query bounds keep their own precision; hidden
    // store nanoseconds must not split rows with the same visible millisecond.
    let millis = nanos / 1_000_000;
    i128::from(awaken_session_application_contract::project_epoch_millis(
        millis,
    )) * 1_000_000
}

#[cfg(kani)]
#[kani::proof]
fn memory_time_filter_compares_visible_milliseconds_without_merging_them() {
    // Direct production-kernel proof over every u128 stored timestamp. The
    // independent UTC maximum below specifies the public millisecond domain;
    // no assumptions hide overflow or submillisecond inputs. Exact quantization
    // means equal buckets cannot split and distinct unsaturated buckets cannot
    // merge. Parser/time-zone and HTTP edges remain executable test obligations.
    let nanos: u128 = kani::any();
    let millis = nanos / 1_000_000;
    let expected = i128::from(millis.min(253_402_300_799_999) as u64) * 1_000_000;
    let visible = projected_time(nanos);
    assert_eq!(visible, expected);
}

fn filter_versions(
    versions: Vec<MemoryVersion>,
    query: &std::collections::HashMap<String, String>,
) -> Result<Vec<MemoryVersion>, String> {
    let operation = query.get("operation").map(String::as_str);
    if operation.is_some_and(|value| !matches!(value, "created" | "modified" | "deleted")) {
        return Err("operation must be `created`, `modified`, or `deleted`".into());
    }
    let created_at_gte = optional_query(query, "created_at[gte]")
        .map(|value| parse_version_time(value, "created_at[gte]"))
        .transpose()?;
    let created_at_lte = optional_query(query, "created_at[lte]")
        .map(|value| parse_version_time(value, "created_at[lte]"))
        .transpose()?;
    Ok(versions
        .into_iter()
        .filter(|version| {
            let created_at = projected_time(version.created_unix_nanos);
            let actor_matches = optional_query(query, "api_key_id").is_none_or(|expected| {
                matches!(&version.created_by, Some(DomainMemoryActor::ApiActor { api_key_id }) if api_key_id == expected)
            }) && optional_query(query, "session_id").is_none_or(|expected| {
                matches!(&version.created_by, Some(DomainMemoryActor::SessionActor { session_id }) if session_id == expected)
            }) && optional_query(query, "service_account_id").is_none_or(|expected| {
                matches!(&version.created_by, Some(DomainMemoryActor::ServiceAccountActor { service_account_id }) if service_account_id == expected)
            });
            actor_matches
                && optional_query(query, "memory_id")
                    .is_none_or(|memory_id| memory_id == version.memory_id.as_str())
                && operation
                    .is_none_or(|operation| operation == version_operation_name(version.operation))
                && created_at_gte.is_none_or(|lower| created_at >= lower)
                && created_at_lte.is_none_or(|upper| created_at <= upper)
        })
        .collect())
}

/// Whether the resource registry contains this store in the trusted Workspace.
async fn list_versions(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    ManagedQuery(query): ManagedQuery<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let view = match MemoryView::parse(&query, MemoryView::Basic) {
        Ok(view) => view,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let page_query = match parse_page_query(&query) {
        Ok(query) => query,
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
    let versions = match filter_versions(collect_versions(&log), &query) {
        Ok(versions) => versions,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let data: Vec<_> = versions
        .iter()
        .map(|version| project_version(version, &id, view))
        .collect();
    (
        StatusCode::OK,
        Json(paginate_by(data, &page_query, |version| version.id.clone())),
    )
        .into_response()
}

async fn get_version(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, vid)): Path<(String, String)>,
    ManagedQuery(query): ManagedQuery<std::collections::HashMap<String, String>>,
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
    _query: ManagedQuery<std::collections::HashMap<String, String>>,
    actor: Option<Extension<AuthenticatedMemoryActor>>,
) -> axum::response::Response {
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    match state
        .memories
        .redact_version_as(&id, &vid, actor.as_ref().map(|actor| &actor.0.0))
        .await
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_outages_are_not_invalid_input_or_missing_memory() {
        // M1 positive missing => 404; M2 identity/config/admission failures =>
        // 400; M3 version exhaustion/concurrent state => 409; M4 unavailable
        // or unknown dependency => 503; M5 corrupt observation or encoding =>
        // 500. One diagnostic string cannot collapse these distinct facts.
        for (error, status) in [
            (
                ResourceRegistryError::NotFound("same".into()),
                StatusCode::NOT_FOUND,
            ),
            (
                ResourceRegistryError::ConfigurationRejected {
                    detail: "same".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                ResourceRegistryError::IdentityRejected {
                    detail: "same".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                ResourceRegistryError::AdmissionRejected {
                    detail: "same".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                ResourceRegistryError::VersionExhausted { id: "same".into() },
                StatusCode::CONFLICT,
            ),
            (
                ResourceRegistryError::AuthorityUnavailable {
                    detail: "same".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ResourceRegistryError::Indeterminate {
                    detail: "same".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ResourceRegistryError::CorruptData("same".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                ResourceRegistryError::EncodingFailure {
                    detail: "same".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(registry_error(error).status(), status);
        }
    }

    #[test]
    fn nullable_memory_page_matches_both_official_sdk_spellings() {
        // The three Memory list operations share parse_page_query. Prove the
        // central parser's TS-empty/Python-omitted equivalence and retain a
        // real cursor, so every caller inherits the same behavior.
        let omitted = parse_page_query(&std::collections::HashMap::new()).unwrap();
        let typescript_null = parse_page_query(&std::collections::HashMap::from([(
            "page".to_owned(),
            String::new(),
        )]))
        .unwrap();
        let cursor = parse_page_query(&std::collections::HashMap::from([(
            "page".to_owned(),
            "memory_1".to_owned(),
        )]))
        .unwrap();
        assert_eq!(typescript_null, omitted);
        assert_eq!(cursor.page.as_deref(), Some("memory_1"));
    }

    #[test]
    fn empty_memory_filters_match_python_omission() {
        // Causal graph: the official TS serializer supplies empty pairs for
        // every optional free-text Memory filter; Python omits them. Exercise
        // the shared lookup plus date parsing and actor/id filtering against a
        // real version so an empty value cannot silently produce zero rows.
        let names = [
            "api_key_id",
            "created_at[gte]",
            "created_at[lte]",
            "memory_id",
            "path_prefix",
            "service_account_id",
            "session_id",
        ];
        let query = names
            .into_iter()
            .map(|name| (name.to_string(), String::new()))
            .collect::<std::collections::HashMap<_, _>>();
        for name in names {
            assert_eq!(optional_query(&query, name), None, "{name}");
        }
        assert_eq!(memory_path_prefix(&query), "/");

        let version = MemoryVersion {
            id: "memver_1".into(),
            memory_id: "mem_1".into(),
            operation: MemoryVersionOperation::Created,
            path: "/memory.md".into(),
            content: Some("content".into()),
            created_unix_nanos: 1_000_000_000,
            created_by: Some(DomainMemoryActor::ApiActor {
                api_key_id: "key_1".into(),
            }),
            redacted_unix_nanos: None,
            redacted_by: None,
        };
        assert_eq!(filter_versions(vec![version], &query).unwrap().len(), 1);
    }

    #[test]
    fn version_time_filters_preserve_public_precision_and_query_boundaries() {
        // Causes: C1 public millisecond bucket (100/101/999/1000), C2 hidden
        // nanoseconds (start/end of bucket), C3 lower/upper/equal/inverted bounds,
        // C4 UTC/equivalent offset, C5 sub-millisecond or pre-epoch query, C6 invalid
        // timestamp. Effects: E1 retain exactly matching visible instants, E2
        // inclusive equality, E3 same public bucket remains inseparable, E4 invalid
        // bound rejects the request. Cross C1*C2*C3 in the table below; C4 must be
        // metamorphically equivalent. Expected membership uses literal millis,
        // independent of the production formatter/parser. No wall-clock sleeps.
        let rows = [100_u128, 101, 999, 1000]
            .into_iter()
            .flat_map(|ms| {
                [0, 999_999].into_iter().map(move |hidden| {
                    (
                        ms,
                        MemoryVersion {
                            id: format!("{ms}-{hidden}"),
                            memory_id: "mem".into(),
                            operation: MemoryVersionOperation::Created,
                            path: "/m".into(),
                            content: None,
                            created_unix_nanos: ms * 1_000_000 + hidden,
                            created_by: None,
                            redacted_unix_nanos: None,
                            redacted_by: None,
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        for (lower, upper, expected_millis) in [
            (None, None, vec![100, 101, 999, 1000]),
            (
                Some("1970-01-01T00:00:00.1Z"),
                Some("1970-01-01T00:00:00.1Z"),
                vec![100],
            ),
            (
                Some("1970-01-01T01:00:00.100+01:00"),
                Some("1969-12-31T19:00:00.100-05:00"),
                vec![100],
            ),
            (
                Some("1970-01-01T00:00:00.100000001Z"),
                None,
                vec![101, 999, 1000],
            ),
            (
                None,
                Some("1970-01-01T00:00:00.999999999Z"),
                vec![100, 101, 999],
            ),
            (
                Some("1970-01-01T00:00:00.101Z"),
                Some("1970-01-01T00:00:00.100Z"),
                vec![],
            ),
            (None, Some("1969-12-31T23:59:59.999Z"), vec![]),
            (Some("1970-01-01T00:00:01Z"), None, vec![1000]),
        ] {
            let query = [("created_at[gte]", lower), ("created_at[lte]", upper)]
                .into_iter()
                .filter_map(|(k, v)| v.map(|v| (k.into(), v.into())))
                .collect();
            let expected = rows
                .iter()
                .filter(|(ms, _)| expected_millis.contains(ms))
                .map(|(_, v)| v.id.clone())
                .collect::<Vec<_>>();
            let actual = filter_versions(rows.iter().map(|(_, v)| v.clone()).collect(), &query)
                .unwrap()
                .into_iter()
                .map(|v| v.id)
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "lower={lower:?}, upper={upper:?}");
        }
        // C7 bridge the actual Memory wire formatter back to query comparison,
        // across bucket edges and both integer narrowing boundaries. The Kani
        // kernels prove numeric projection; this checks their codec call wiring.
        for nanos in [
            0,
            999_999,
            1_000_000,
            100_999_999,
            101_000_000,
            253_402_300_799_999_999_999,
            253_402_300_800_000_000_000,
            u128::from(u64::MAX) * 1_000_000,
            u128::MAX,
        ] {
            assert_eq!(
                parse_version_time(&timestamp(nanos), "created_at").unwrap(),
                projected_time(nanos),
                "C7 nanos={nanos}"
            );
        }
        for key in ["created_at[gte]", "created_at[lte]"] {
            let query = std::collections::HashMap::from([(key.into(), "not-a-time".into())]);
            assert!(
                filter_versions(vec![], &query).unwrap_err().contains(key),
                "C6/E4"
            );
        }
    }

    #[test]
    fn memory_list_union_and_delete_receipts_have_closed_wire_shapes() {
        // Cause/effect decision table: M1 stored memory -> full typed memory
        // fields without the internal complete-generation CAS token; M2
        // depth rollup -> prefix-only fields; M3 deletion -> id/type receipt.
        // The untagged enum selects one DTO and never merges the two variants
        // or relies on object indexing.
        let memory = serde_json::to_value(MemoryListItem::Memory(MemoryObject {
            id: "mem_1".into(),
            kind: "memory",
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            memory_store_id: "memstore_1".into(),
            memory_version_id: "memver_1".into(),
            path: "/notes.md".into(),
            content: None,
            content_sha256: "0".repeat(64),
            content_size_bytes: 0,
        }))
        .unwrap();
        assert_eq!(memory["type"], "memory", "M1");
        assert!(
            memory.get("generation").is_none(),
            "M1 internal CAS is not wire data"
        );
        assert!(memory["content"].is_null(), "M1 basic view");
        let prefix = serde_json::to_value(MemoryListItem::Prefix(MemoryPrefix {
            kind: "memory_prefix",
            path: "/projects/".into(),
        }))
        .unwrap();
        assert_eq!(prefix["type"], "memory_prefix", "M2");
        assert_eq!(prefix.as_object().unwrap().len(), 2, "M2 exact fields");
        let deleted = serde_json::to_value(DeletedMemoryResource {
            id: "mem_1".into(),
            kind: "memory_deleted",
        })
        .unwrap();
        assert_eq!(deleted["type"], "memory_deleted", "M3");
        assert_eq!(deleted.as_object().unwrap().len(), 2, "M3 exact fields");
    }
}
