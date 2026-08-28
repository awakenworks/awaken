//! The skills API (`/v1/skills`) over the host's durable delivered-skill catalog
//! (ADR-0036) plus the official `@anthropic-ai/sdk` `beta.skills.*` surface:
//! multipart create, retrieve, list, delete, and the `versions` subresource
//! (create / retrieve / list / delete / get-content).
//!
//! The SDK multipart upload feeds the runtime's delivered-skill catalog
//! ([`awaken_resource_contract::SkillStore`]), so an uploaded skill is offered on
//! selected threads and survives a restart. The SDK object, immutable versions,
//! and binary bundle share that one Workspace-scoped repository.

use std::sync::Arc;

use crate::common::headers::ManagedCapability;
use crate::common::scope::RequiredWorkspaceScope;
use crate::resources::flavor::{
    BetaQueryPolicy, ManagedResourceApiFlavor, resource_api_flavor, without_beta_selector,
};
use crate::types::skill::{
    BetaSkill, BetaSkillListParams, BetaSkillVersion, BetaSkillVersionListParams, DeletedSkill,
    DeletedSkillObjectType, DeletedSkillVersion, DeletedSkillVersionObjectType, Skill,
    SkillListParams, SkillObjectType, SkillSource, SkillVersion as SkillVersionDto,
    SkillVersionListParams, SkillVersionObjectType, SkillVersionWire, SkillWire,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate};
use awaken_resource_application::{
    CanonicalSkillBundle, MAX_SKILL_ARCHIVE_BYTES, MAX_SKILL_FILES, UploadedSkillBundleFile,
    canonicalize_skill_bundle, normalize_bundle_path,
};
use awaken_resource_contract::{
    ResourceKind, ResourceTarget, SkillDefinition, SkillStore, SkillStoreError, SkillVersion,
    skill_bundle_sha256, skill_catalog_id, skill_stem,
};
use axum::extract::{Multipart, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

fn timestamp(nanos: u64) -> String {
    awaken_session_contract::epoch_millis_to_rfc3339(nanos / 1_000_000)
}

fn project_definition(
    definition: &SkillDefinition,
    latest: &SkillVersion,
    flavor: ManagedResourceApiFlavor,
) -> SkillWire {
    match flavor {
        ManagedResourceApiFlavor::Beta => SkillWire::Beta(BetaSkill {
            id: definition.id.to_string(),
            kind: SkillObjectType::Skill,
            created_at: timestamp(definition.timestamps.created_unix_nanos),
            updated_at: timestamp(definition.timestamps.updated_unix_nanos),
            display_title: definition.display_title.clone(),
            latest_version: Some(definition.latest_version.to_string()),
            source: "custom",
        }),
        ManagedResourceApiFlavor::Ga => SkillWire::Ga(Skill {
            id: definition.id.to_string(),
            kind: SkillObjectType::Skill,
            created_at: timestamp(definition.timestamps.created_unix_nanos),
            updated_at: timestamp(definition.timestamps.updated_unix_nanos),
            display_name: definition
                .display_title
                .clone()
                .unwrap_or_else(|| latest.name.clone()),
            latest_version_id: latest.id.to_string(),
            source: SkillSource::Custom,
        }),
    }
}

fn project_version(version: &SkillVersion, flavor: ManagedResourceApiFlavor) -> SkillVersionWire {
    match flavor {
        ManagedResourceApiFlavor::Beta => SkillVersionWire::Beta(BetaSkillVersion {
            id: version.id.to_string(),
            kind: SkillVersionObjectType::SkillVersion,
            created_at: timestamp(version.created_unix_nanos),
            description: version.description.clone(),
            directory: version.directory.clone(),
            name: version.name.clone(),
            skill_id: version.skill_id.to_string(),
            version: version.version.to_string(),
        }),
        ManagedResourceApiFlavor::Ga => SkillVersionWire::Ga(SkillVersionDto {
            id: version.id.to_string(),
            kind: SkillVersionObjectType::SkillVersion,
            created_at: timestamp(version.created_unix_nanos),
            description: version.description.clone(),
            name: version.name.clone(),
            skill_id: version.skill_id.to_string(),
        }),
    }
}

/// Skills API state. The durable repository is the only resource truth; there is
/// deliberately no HTTP-local registry or authorization data here.
struct SkillsApi {
    store: Option<Arc<dyn SkillStore>>,
    purge: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
}

impl SkillsApi {
    async fn create(
        &self,
        definition: SkillDefinition,
        version: SkillVersion,
    ) -> Option<Result<(), SkillStoreError>> {
        Some(self.store.as_ref()?.create(definition, version).await)
    }

    async fn append_version(
        &self,
        workspace: &str,
        id: &str,
        version: SkillVersion,
    ) -> Option<Result<(), SkillStoreError>> {
        Some(
            self.store
                .as_ref()?
                .append_version(workspace, id, version)
                .await,
        )
    }

    async fn definition(
        &self,
        workspace: &str,
        id: &str,
    ) -> Option<Result<Option<SkillDefinition>, SkillStoreError>> {
        Some(self.store.as_ref()?.definition(workspace, id).await)
    }

    async fn definitions(&self, workspace: &str) -> Result<Vec<SkillDefinition>, SkillStoreError> {
        match &self.store {
            Some(store) => store.list_definitions(workspace).await,
            None => Ok(Vec::new()),
        }
    }

    async fn versions(
        &self,
        workspace: &str,
        id: &str,
    ) -> Option<Result<Vec<SkillVersion>, SkillStoreError>> {
        Some(self.store.as_ref()?.list_versions(workspace, id).await)
    }

    async fn delete(&self, workspace: &str, id: &str) -> Option<Result<bool, SkillStoreError>> {
        Some(self.store.as_ref()?.delete_skill(workspace, id).await)
    }

    async fn delete_version(
        &self,
        workspace: &str,
        id: &str,
        version: u64,
    ) -> Option<Result<bool, SkillStoreError>> {
        Some(
            self.store
                .as_ref()?
                .delete_version(workspace, id, version)
                .await,
        )
    }
}

/// Mount the skills API over the host's durable skill catalog.
pub fn skills_router(
    store: Option<Arc<dyn SkillStore>>,
    purge: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
) -> Router {
    let state = Arc::new(SkillsApi { store, purge });
    Router::new()
        .route("/v1/skills", post(create_skill).get(list_skills))
        .route("/v1/skills/{id}", get(retrieve_skill).delete(delete_skill))
        .route(
            "/v1/skills/{id}/versions",
            post(create_version).get(list_versions),
        )
        .route(
            "/v1/skills/{id}/versions/{version}",
            get(retrieve_version).delete(delete_version),
        )
        .route(
            "/v1/skills/{id}/versions/{version}/content",
            get(version_content),
        )
        .route(
            "/v1/skills/{id}/versions/{version}/files/{*path}",
            get(version_file),
        )
        .with_state(state)
}

fn err(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    let error_type = if status == StatusCode::NOT_FOUND {
        "not_found_error"
    } else if status.is_server_error() {
        "api_error"
    } else {
        "invalid_request_error"
    };
    (status, Json(ErrorResponse::new(error_type, message))).into_response()
}

/// Collect a multipart body without decoding file bytes. Non-file text fields are
/// read for `display_title`; bundle contents remain binary-safe end to end.
async fn read_multipart(
    mut multipart: Multipart,
) -> Result<(Option<String>, Vec<UploadedSkillBundleFile>), String> {
    let mut display_title = None;
    let mut executable_paths = None;
    let mut files = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| error.to_string())?
    {
        let name = field.name().map(str::to_string);
        let filename = field.file_name().map(str::to_string);
        if let Some(fname) = filename {
            let bytes = field.bytes().await.map_err(|error| error.to_string())?;
            if bytes.len() > MAX_SKILL_ARCHIVE_BYTES {
                return Err(format!(
                    "skill upload file exceeds {MAX_SKILL_ARCHIVE_BYTES} bytes"
                ));
            }
            files.push(UploadedSkillBundleFile {
                path: fname,
                content: bytes.to_vec(),
                executable: false,
            });
        } else if matches!(name.as_deref(), Some("display_title" | "display_name")) {
            if display_title.is_some() {
                return Err("display_name/display_title may be supplied only once".into());
            }
            display_title = Some(field.text().await.map_err(|error| error.to_string())?);
        } else if name.as_deref() == Some("executable_paths") {
            if executable_paths.is_some() {
                return Err("executable_paths may be supplied only once".into());
            }
            let bytes = field.bytes().await.map_err(|error| error.to_string())?;
            if bytes.len() > 16 * 1024 {
                return Err("executable_paths metadata is too large".into());
            }
            let paths: Vec<String> = serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid executable_paths metadata: {error}"))?;
            if paths.len() > MAX_SKILL_FILES {
                return Err(format!(
                    "executable_paths exceeds {MAX_SKILL_FILES} entries"
                ));
            }
            executable_paths = Some(paths);
        }
    }
    if let Some(paths) = executable_paths {
        if files.len() == 1 && files[0].path.to_ascii_lowercase().ends_with(".zip") {
            return Err("executable_paths cannot override ZIP permissions".into());
        }
        for raw_path in paths {
            let path = normalize_bundle_path(&raw_path)?;
            let mut matched = false;
            for file in &mut files {
                if normalize_bundle_path(&file.path)? == path {
                    file.executable = true;
                    matched = true;
                }
            }
            if !matched {
                return Err(format!(
                    "executable path `{raw_path}` is absent from the uploaded bundle"
                ));
            }
        }
    }
    Ok((display_title, files))
}

fn bundle_skill_md(bundle: &CanonicalSkillBundle) -> Result<&str, String> {
    bundle
        .files
        .iter()
        .find(|file| file.path == "SKILL.md")
        .ok_or_else(|| "skill upload has no SKILL.md at the bundle root".to_string())
        .and_then(|file| {
            std::str::from_utf8(&file.content)
                .map_err(|_| "SKILL.md must be valid UTF-8".to_string())
        })
}

/// Build a version's projected metadata from SKILL.md content (parse frontmatter
/// for name + description). Delivery to the durable catalog is the caller's job —
/// the SDK path delivers under the skill's name, the legacy path under its id — so
/// a skill is stored under exactly one durable id (no double-write).
fn build_version(
    skill_id: &str,
    content: &str,
    ordinal: u64,
    bundle: CanonicalSkillBundle,
) -> SkillVersion {
    let spec = awaken_ext_skills::parse_skill_md("skill", content);
    let files = bundle.files;
    SkillVersion {
        id: format!("skver_{}_{ordinal}", skill_stem(skill_id)).into(),
        skill_id: skill_id.into(),
        version: ordinal,
        name: spec.name.clone(),
        description: spec.description.clone(),
        directory: bundle
            .source_directory
            .unwrap_or_else(|| skill_stem(&spec.name)),
        bundle_sha256: skill_bundle_sha256(&files),
        files,
        created_unix_nanos: now_nanos(),
    }
}

fn store_error(error: SkillStoreError) -> axum::response::Response {
    match error {
        SkillStoreError::AlreadyExists(_) | SkillStoreError::VersionConflict(_) => {
            err(StatusCode::CONFLICT, error.to_string())
        }
        SkillStoreError::NotFound(_) => err(StatusCode::NOT_FOUND, error.to_string()),
        SkillStoreError::Invalid(_) => err(StatusCode::BAD_REQUEST, error.to_string()),
        SkillStoreError::Io(_) | SkillStoreError::Storage(_) => {
            err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    }
}

// ---- Skill routes ----------------------------------------------------------

/// `POST /v1/skills` — the SDK multipart upload of SKILL.md plus support files.
async fn create_skill(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    multipart: Multipart,
) -> axum::response::Response {
    let flavor = match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Skills,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        Ok(flavor) => flavor,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let (display_title, files) = match read_multipart(multipart).await {
        Ok(upload) => upload,
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    let bundle = match canonicalize_skill_bundle(files) {
        Ok(bundle) => bundle,
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    let content = match bundle_skill_md(&bundle) {
        Ok(content) => content.to_string(),
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    let parsed = awaken_ext_skills::parse_skill_md("skill", &content);
    let id = skill_catalog_id(&parsed.name);
    let version = build_version(&id, &content, 1, bundle);
    let definition = SkillDefinition {
        id: id.clone().into(),
        workspace_id: workspace,
        display_title,
        latest_version: 1,
        last_version: 1,
        timestamps: awaken_resource_contract::ResourceTimestamps::created(
            version.created_unix_nanos,
        ),
    };
    let projected = project_definition(&definition, &version, flavor);
    let Some(result) = state.create(definition.clone(), version).await else {
        return err(
            StatusCode::CONFLICT,
            "this server has no durable skill store",
        );
    };
    match result {
        Ok(()) => (StatusCode::OK, Json(projected)).into_response(),
        Err(error) => store_error(error),
    }
}

/// `GET /v1/skills` — the registered skills as a cursor page. Includes durable
/// delivered skills the in-memory registry does not track (e.g. after a restart,
/// when the registry is empty but the durable catalog persists), so a skill
/// uploaded before a restart still lists.
async fn list_skills(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> axum::response::Response {
    let flavor = match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Skills,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        Ok(flavor) => flavor,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let (page, source) = match flavor {
        ManagedResourceApiFlavor::Beta => {
            match serde_urlencoded::from_str::<BetaSkillListParams>(&without_beta_selector(
                raw.as_deref(),
            )) {
                Ok(query) => (
                    PageQuery {
                        page: query.page,
                        limit: query.limit.map(usize::from),
                    },
                    query.source,
                ),
                Err(error) => return err(StatusCode::BAD_REQUEST, error.to_string()),
            }
        }
        ManagedResourceApiFlavor::Ga => {
            match serde_urlencoded::from_str::<SkillListParams>(&without_beta_selector(
                raw.as_deref(),
            )) {
                Ok(query) => (
                    PageQuery {
                        page: query.page,
                        limit: query.limit.map(usize::from),
                    },
                    query.source,
                ),
                Err(error) => return err(StatusCode::BAD_REQUEST, error.to_string()),
            }
        }
    };
    if source
        .as_deref()
        .is_some_and(|source| !matches!(source, "custom" | "anthropic"))
    {
        return err(
            StatusCode::BAD_REQUEST,
            "source must be `custom` or `anthropic`",
        );
    }
    let definitions = match state.definitions(&workspace).await {
        Ok(definitions) => definitions,
        Err(error) => return store_error(error),
    };
    if source.as_deref() == Some("anthropic") {
        return (
            StatusCode::OK,
            Json(PageCursor::<SkillWire>::single(Vec::new())),
        )
            .into_response();
    }
    let mut data = Vec::with_capacity(definitions.len());
    for definition in &definitions {
        let versions = match state.versions(&workspace, definition.id.as_str()).await {
            Some(Ok(versions)) => versions,
            Some(Err(error)) => return store_error(error),
            None => Vec::new(),
        };
        let Some(latest) = versions
            .iter()
            .find(|version| version.version == definition.latest_version)
        else {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "skill latest version is missing",
            );
        };
        data.push(project_definition(definition, latest, flavor));
    }
    (
        StatusCode::OK,
        Json(paginate(data, &page, |skill| match skill {
            SkillWire::Beta(skill) => skill.id.as_str(),
            SkillWire::Ga(skill) => skill.id.as_str(),
        })),
    )
        .into_response()
}

async fn retrieve_skill(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> axum::response::Response {
    let flavor = match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Skills,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        Ok(flavor) => flavor,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    match state.definition(&workspace, &id).await {
        Some(Ok(Some(definition))) => match find_version(&state, &workspace, &id, "latest").await {
            Ok(Some(latest)) => (
                StatusCode::OK,
                Json(project_definition(&definition, &latest, flavor)),
            )
                .into_response(),
            Ok(None) => err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "skill latest version is missing",
            ),
            Err(error) => store_error(error),
        },
        Some(Err(error)) => store_error(error),
        None => err(StatusCode::NOT_FOUND, format!("skill `{id}` not found")),
        Some(Ok(None)) => err(StatusCode::NOT_FOUND, format!("skill `{id}` not found")),
    }
}

async fn delete_skill(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> axum::response::Response {
    if let Err(message) = resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Skills,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        return err(StatusCode::BAD_REQUEST, message);
    }
    let definition = match state.definition(&workspace, &id).await {
        Some(Ok(Some(definition))) => definition,
        Some(Err(error)) => return store_error(error),
        Some(Ok(None)) | None => {
            return err(StatusCode::NOT_FOUND, format!("skill `{id}` not found"));
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    if let Err(error) = state
        .purge
        .schedule_purge(
            ResourceTarget::new(&workspace, ResourceKind::Skill, &id),
            Some(definition.latest_version),
            now,
            now,
        )
        .await
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    match state.delete(&workspace, &id).await {
        Some(Ok(true)) => (
            StatusCode::OK,
            Json(DeletedSkill {
                id,
                kind: DeletedSkillObjectType::SkillDeleted,
            }),
        )
            .into_response(),
        Some(Ok(false)) | None => err(StatusCode::NOT_FOUND, format!("skill `{id}` not found")),
        Some(Err(error)) => store_error(error),
    }
}

// ---- Version routes --------------------------------------------------------

async fn create_version(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    multipart: Multipart,
) -> axum::response::Response {
    let flavor = match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Skills,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        Ok(flavor) => flavor,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let definition = match state.definition(&workspace, &id).await {
        Some(Ok(Some(definition))) => definition,
        Some(Err(error)) => return store_error(error),
        Some(Ok(None)) | None => {
            return err(StatusCode::NOT_FOUND, format!("skill `{id}` not found"));
        }
    };
    if let Some(expected) = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_matches('"'))
        && expected != definition.latest_version.to_string()
    {
        return err(
            StatusCode::CONFLICT,
            format!(
                "skill `{id}` changed: expected version {expected}, latest is {}",
                definition.latest_version
            ),
        );
    }
    let (_title, files) = match read_multipart(multipart).await {
        Ok(upload) => upload,
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    let bundle = match canonicalize_skill_bundle(files) {
        Ok(bundle) => bundle,
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    let content = match bundle_skill_md(&bundle) {
        Ok(content) => content.to_string(),
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    let version = build_version(&id, &content, definition.last_version + 1, bundle);
    let projected = project_version(&version, flavor);
    match state.append_version(&workspace, &id, version).await {
        Some(Ok(())) => (StatusCode::OK, Json(projected)).into_response(),
        Some(Err(error)) => store_error(error),
        None => err(
            StatusCode::CONFLICT,
            "this server has no durable skill store",
        ),
    }
}

async fn list_versions(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> axum::response::Response {
    let flavor = match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Skills,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        Ok(flavor) => flavor,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let page = match flavor {
        ManagedResourceApiFlavor::Beta => serde_urlencoded::from_str::<BetaSkillVersionListParams>(
            &without_beta_selector(raw.as_deref()),
        )
        .map(|query| PageQuery {
            page: query.page,
            limit: query.limit.map(usize::from),
        }),
        ManagedResourceApiFlavor::Ga => serde_urlencoded::from_str::<SkillVersionListParams>(
            &without_beta_selector(raw.as_deref()),
        )
        .map(|query| PageQuery {
            page: query.page,
            limit: query.limit.map(usize::from),
        }),
    };
    let page = match page {
        Ok(page) => page,
        Err(error) => return err(StatusCode::BAD_REQUEST, error.to_string()),
    };
    match state.versions(&workspace, &id).await {
        Some(Ok(versions)) if !versions.is_empty() => {
            let data: Vec<_> = versions
                .iter()
                .map(|version| project_version(version, flavor))
                .collect();
            (
                StatusCode::OK,
                Json(paginate(data, &page, |version| match version {
                    SkillVersionWire::Beta(version) => version.id.as_str(),
                    SkillVersionWire::Ga(version) => version.id.as_str(),
                })),
            )
                .into_response()
        }
        Some(Err(error)) => store_error(error),
        Some(Ok(_)) | None => err(StatusCode::NOT_FOUND, format!("skill `{id}` not found")),
    }
}

async fn find_version(
    state: &SkillsApi,
    workspace: &str,
    skill_id: &str,
    reference: &str,
) -> Result<Option<SkillVersion>, SkillStoreError> {
    let versions = state
        .versions(workspace, skill_id)
        .await
        .unwrap_or_else(|| Ok(Vec::new()))?;
    if reference == "latest" {
        return Ok(versions.into_iter().last());
    }
    Ok(versions.into_iter().find(|version| {
        version.version.to_string() == reference || version.id.as_str() == reference
    }))
}

async fn retrieve_version(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, version)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> axum::response::Response {
    let flavor = match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Skills,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        Ok(flavor) => flavor,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    match find_version(&state, &workspace, &id, &version).await {
        Ok(Some(version)) => {
            (StatusCode::OK, Json(project_version(&version, flavor))).into_response()
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "skill version not found"),
        Err(error) => store_error(error),
    }
}

async fn delete_version(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, version)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> axum::response::Response {
    if let Err(message) = resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Skills,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        return err(StatusCode::BAD_REQUEST, message);
    }
    let found = match find_version(&state, &workspace, &id, &version).await {
        Ok(Some(version)) => version,
        Ok(None) => return err(StatusCode::NOT_FOUND, "skill version not found"),
        Err(error) => return store_error(error),
    };
    match state.delete_version(&workspace, &id, found.version).await {
        Some(Ok(true)) => (
            StatusCode::OK,
            Json(DeletedSkillVersion {
                id: found.id.to_string(),
                kind: DeletedSkillVersionObjectType::SkillVersionDeleted,
            }),
        )
            .into_response(),
        Some(Ok(false)) | None => err(StatusCode::NOT_FOUND, "skill version not found"),
        Some(Err(error)) => store_error(error),
    }
}

fn version_archive(version: &SkillVersion) -> Result<Vec<u8>, String> {
    let mut archive = tar::Builder::new(Vec::new());
    let wrapper = skill_stem(&version.name);
    for file in &version.files {
        let mut entry = tar::Header::new_gnu();
        entry.set_size(file.content.len() as u64);
        entry.set_mode(if file.executable { 0o755 } else { 0o644 });
        entry.set_uid(0);
        entry.set_gid(0);
        entry.set_mtime(version.created_unix_nanos / 1_000_000_000);
        entry.set_cksum();
        archive
            .append_data(
                &mut entry,
                format!("{wrapper}/{}", file.path),
                file.content.as_slice(),
            )
            .map_err(|error| format!("build Skill version archive: {error}"))?;
    }
    archive
        .into_inner()
        .map_err(|error| format!("finish Skill version archive: {error}"))
}

/// `GET /v1/skills/:id/versions/:version/content` — the immutable bundle as a
/// tar archive. The official `setupSkills` helper consumes this endpoint through
/// `skills.versions.download()` and requires an archive, not a bare SKILL.md.
async fn version_content(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, version)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> axum::response::Response {
    match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Skills,
        BetaQueryPolicy::QuerySelectsBeta,
    ) {
        Ok(ManagedResourceApiFlavor::Beta) => {}
        Ok(ManagedResourceApiFlavor::Ga) => {
            return err(StatusCode::NOT_FOUND, "GA Skills has no content endpoint");
        }
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    }
    match find_version(&state, &workspace, &id, &version).await {
        Ok(Some(version)) => match version_archive(&version) {
            Ok(content) => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-tar"),
                );
                headers.insert(
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_static("attachment; filename=skill.tar"),
                );
                (StatusCode::OK, headers, content).into_response()
            }
            Err(error) => err(StatusCode::INTERNAL_SERVER_ERROR, error),
        },
        Ok(None) => err(StatusCode::NOT_FOUND, "skill version not found"),
        Err(error) => store_error(error),
    }
}

/// Retrieve one support file from the immutable uploaded bundle. Traversal and
/// absolute paths are rejected by the same normalization used at ingestion.
async fn version_file(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, version, path)): Path<(String, String, String)>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> axum::response::Response {
    match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Skills,
        BetaQueryPolicy::QuerySelectsBeta,
    ) {
        Ok(ManagedResourceApiFlavor::Beta) => {}
        Ok(ManagedResourceApiFlavor::Ga) => {
            return err(StatusCode::NOT_FOUND, "GA Skills has no file endpoint");
        }
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    }
    let normalized = match normalize_bundle_path(&path) {
        Ok(path) => path,
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    match find_version(&state, &workspace, &id, &version).await {
        Ok(Some(version)) => match version.files.iter().find(|file| file.path == normalized) {
            Some(file) => (StatusCode::OK, file.content.clone()).into_response(),
            None => err(StatusCode::NOT_FOUND, "skill version file not found"),
        },
        Ok(None) => err(StatusCode::NOT_FOUND, "skill version not found"),
        Err(error) => store_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_skill_store::InMemorySkillStore;
    use awaken_tenancy::WorkspaceScope;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn skill_and_version_dtos_emit_only_official_response_fields() {
        // Cause/effect decision table: S1 Skill projection -> seven official
        // fields; S2 Version projection -> eight official fields; S3 deletion ->
        // id/type only. Durable bundle hashes, file manifests, and executable
        // flags remain repository facts and do not become a parallel wire API.
        let definition = SkillDefinition {
            id: "skill_1".into(),
            workspace_id: "workspace".into(),
            display_title: Some("Skill".into()),
            latest_version: 2,
            last_version: 2,
            timestamps: awaken_resource_contract::ResourceTimestamps::created(1_000_000),
        };
        let version = SkillVersion {
            id: "skver_1".into(),
            skill_id: "skill_1".into(),
            version: 2,
            name: "skill".into(),
            description: "description".into(),
            directory: "skill".into(),
            bundle_sha256: "sha256:test".into(),
            files: Vec::new(),
            created_unix_nanos: 1_000_000,
        };
        let beta_skill = serde_json::to_value(project_definition(
            &definition,
            &version,
            ManagedResourceApiFlavor::Beta,
        ))
        .unwrap();
        assert_eq!(beta_skill.as_object().unwrap().len(), 7, "S1 beta");
        let ga_skill = serde_json::to_value(project_definition(
            &definition,
            &version,
            ManagedResourceApiFlavor::Ga,
        ))
        .unwrap();
        assert_eq!(ga_skill.as_object().unwrap().len(), 7, "S1 GA");
        assert!(ga_skill["source"].is_object(), "S1 GA source object");
        assert_eq!(ga_skill["latest_version_id"], "skver_1", "S1 GA");

        let beta_version =
            serde_json::to_value(project_version(&version, ManagedResourceApiFlavor::Beta))
                .unwrap();
        assert_eq!(beta_version.as_object().unwrap().len(), 8, "S2 beta");
        let ga_version =
            serde_json::to_value(project_version(&version, ManagedResourceApiFlavor::Ga)).unwrap();
        assert_eq!(ga_version.as_object().unwrap().len(), 6, "S2 GA");
        assert!(ga_version.get("directory").is_none(), "S2 no beta fields");
        assert!(ga_version.get("files").is_none(), "S2 no extension fields");
        let deleted = serde_json::to_value(DeletedSkill {
            id: "skill_1".into(),
            kind: DeletedSkillObjectType::SkillDeleted,
        })
        .unwrap();
        assert_eq!(deleted.as_object().unwrap().len(), 2, "S3");
    }

    fn purge_scheduler() -> Arc<dyn awaken_resource_contract::ResourcePurgeScheduler> {
        Arc::new(awaken_resource_application::RepositoryPurgeScheduler::new(
            Arc::new(
                awaken_resource_store::SqliteResourceStore::in_memory()
                    .expect("resource lifecycle"),
            ),
        ))
    }

    async fn get(router: &Router, uri: &str) -> (StatusCode, String) {
        let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(WorkspaceScope("test".into()));
        let resp = router.clone().oneshot(request).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn get_in(router: &Router, uri: &str, workspace: &str) -> (StatusCode, String) {
        let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(WorkspaceScope(workspace.to_string()));
        let resp = router.clone().oneshot(request).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn get_beta_in(router: &Router, uri: &str, workspace: &str) -> (StatusCode, String) {
        let separator = if uri.contains('?') { '&' } else { '?' };
        let mut request = Request::builder()
            .uri(format!("{uri}{separator}beta=true"))
            .header("anthropic-beta", ManagedCapability::Skills.beta())
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(WorkspaceScope(workspace.to_string()));
        let resp = router.clone().oneshot(request).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn persist_directly(store: &dyn SkillStore, workspace: &str, id: &str, content: &str) {
        let bundle = canonicalize_skill_bundle(vec![UploadedSkillBundleFile {
            path: "SKILL.md".into(),
            content: content.as_bytes().to_vec(),
            executable: false,
        }])
        .unwrap();
        let version = build_version(id, content, 1, bundle);
        store
            .create(
                SkillDefinition {
                    id: id.into(),
                    workspace_id: workspace.into(),
                    display_title: None,
                    latest_version: 1,
                    last_version: 1,
                    timestamps: Default::default(),
                },
                version,
            )
            .await
            .unwrap();
    }

    /// Cause/effect graph: identical Skill ids in distinct Workspaces are
    /// distinct aggregates. A selected Workspace may read its value and list;
    /// another Workspace receives no identity or content disclosure.
    ///
    /// | Rule | Stored in A | Selected Workspace | Effect |
    /// |---|---|---|---|
    /// | S1 | yes | A | retrieve succeeds |
    /// | S2 | yes | B | retrieve 404; list omits id |
    #[tokio::test]
    async fn the_same_skill_id_is_isolated_by_workspace() {
        let store: Arc<dyn SkillStore> = Arc::new(InMemorySkillStore::new());
        persist_directly(
            store.as_ref(),
            "ws_a",
            "private",
            "---\nname: private\ndescription: a\n---\nsecret-a",
        )
        .await;
        let router = skills_router(Some(store), purge_scheduler());
        let id = "private";
        assert_eq!(
            get_in(&router, &format!("/v1/skills/{id}"), "ws_a").await.0,
            StatusCode::OK
        );
        assert_eq!(
            get_in(&router, &format!("/v1/skills/{id}"), "ws_b").await.0,
            StatusCode::NOT_FOUND
        );
        assert!(
            !get_in(&router, "/v1/skills", "ws_b")
                .await
                .1
                .contains("private")
        );
    }

    /// Cause/effect graph: a Skill committed directly to the authoritative
    /// repository, without HTTP-local state, must remain retrievable/listable by
    /// the same stable id; an unknown id remains 404.
    ///
    /// | Rule | Durable aggregate | Requested id | Effect |
    /// |---|---|---|---|
    /// | D1 | present | exact/latest | metadata and content returned |
    /// | D2 | present | unknown | 404 |
    #[tokio::test]
    async fn a_durable_only_skill_resolves_by_its_catalog_id() {
        let store: Arc<dyn SkillStore> = Arc::new(InMemorySkillStore::new());
        let workspace = "test".to_string();
        // Deliver straight to the durable repository — the API reads the same truth.
        persist_directly(
            store.as_ref(),
            &workspace,
            "Greeter",
            "---\nname: Greeter\ndescription: hi\n---\nsay hello",
        )
        .await;
        let router = skills_router(Some(store), purge_scheduler());
        let cid = "Greeter";

        // The advertised catalog id retrieves the skill via the durable fallback…
        let (status, _) = get_in(&router, &format!("/v1/skills/{cid}"), &workspace).await;
        assert_eq!(status, StatusCode::OK, "catalog id retrieves the skill");
        // …and `version: "latest"` downloads its content (what the worker fetches).
        let (status, body) = get_beta_in(
            &router,
            &format!("/v1/skills/{cid}/versions/latest/content"),
            &workspace,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("say hello"),
            "latest content downloads: {body}"
        );
        // The list surfaces the durable skill under its stable stem (the durability
        // contract). The tagged catalog id is what advertisement offers the worker,
        // and both forms resolve on retrieve/download (asserted above).
        let (_, list) = get_in(&router, "/v1/skills", &workspace).await;
        assert!(
            list.contains("Greeter"),
            "list surfaces the durable stem: {list}"
        );
        // An unknown id is a clean 404.
        let (status, _) = get(&router, "/v1/skills/skill_deadbeefdeadbeef").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
