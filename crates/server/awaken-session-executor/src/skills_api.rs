//! The skills API (`/v1/skills`) over the host's durable delivered-skill catalog
//! (ADR-0036) plus the official `@anthropic-ai/sdk` `beta.skills.*` surface:
//! multipart create, retrieve, list, delete, and the `versions` subresource
//! (create / retrieve / list / delete / get-content).
//!
//! Two create paths coexist so nothing regresses: a JSON body `{id, content}`
//! keeps the original delivery contract (used by the resource-mount e2e), while a
//! multipart upload is the SDK path. BOTH feed the runtime's delivered-skill
//! catalog ([`awaken_skill_store::SkillStore`]) so a skill created either way is
//! offered on selected threads and survives a restart. The SDK object, immutable
//! versions, and binary bundle share that one Workspace-scoped repository; the
//! former API-local registry is migration input only.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_protocol_managed::resource_plane::{ResourceKind, ResourceTarget};
use awaken_skill_store::{SkillBundleFile, SkillDefinition, SkillStoreError, SkillVersion};
use axum::extract::{FromRequest, Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::resource_scope::RequiredWorkspaceScope;

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

fn timestamp(nanos: u64) -> String {
    awaken_protocol_managed::cron::to_rfc3339(nanos / 1_000_000)
}

fn project_definition(definition: &SkillDefinition) -> Value {
    json!({
        "id": definition.id,
        "type": "skill",
        "created_at": timestamp(definition.timestamps.created_unix_nanos),
        "updated_at": timestamp(definition.timestamps.updated_unix_nanos),
        "display_title": definition.display_title,
        "latest_version": definition.latest_version.to_string(),
        "source": "api",
    })
}

fn project_version(version: &SkillVersion) -> Value {
    json!({
        "id": version.id,
        "type": "skill_version",
        "created_at": timestamp(version.created_unix_nanos),
        "description": version.description,
        "directory": version.directory,
        "name": version.name,
        "skill_id": version.skill_id,
        "version": version.version.to_string(),
        "files": version.files.iter().map(|file| &file.path).collect::<Vec<_>>(),
        "bundle_sha256": version.bundle_sha256,
    })
}

/// Skills API state. The durable repository is the only resource truth; there is
/// deliberately no HTTP-local registry or authorization data here.
struct SkillsApi {
    store: Option<Arc<dyn awaken_skill_store::SkillStore>>,
    purge: Arc<dyn awaken_protocol_managed::resource_plane::ResourcePurgeScheduler>,
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
    store: Option<Arc<dyn awaken_skill_store::SkillStore>>,
    purge: Arc<dyn awaken_protocol_managed::resource_plane::ResourcePurgeScheduler>,
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
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// Collect a multipart body without decoding file bytes. Non-file text fields are
/// read for `display_title`; bundle contents remain binary-safe end to end.
async fn read_multipart(mut multipart: Multipart) -> (Option<String>, Vec<(String, Vec<u8>)>) {
    let mut display_title = None;
    let mut files = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().map(str::to_string);
        let filename = field.file_name().map(str::to_string);
        if let Some(fname) = filename {
            if let Ok(bytes) = field.bytes().await {
                files.push((fname, bytes.to_vec()));
            }
        } else if name.as_deref() == Some("display_title") {
            display_title = field.text().await.ok();
        }
    }
    (display_title, files)
}

fn normalize_bundle(files: Vec<(String, Vec<u8>)>) -> Result<BTreeMap<String, Vec<u8>>, String> {
    const MAX_FILES: usize = 128;
    const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    if files.len() > MAX_FILES {
        return Err(format!("skill bundle exceeds {MAX_FILES} files"));
    }
    let mut total = 0usize;
    let mut bundle = BTreeMap::new();
    for (raw, content) in files {
        let path = raw.replace('\\', "/");
        if path.starts_with('/')
            || path
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            return Err(format!("invalid skill bundle path `{raw}`"));
        }
        if content.len() > MAX_FILE_BYTES {
            return Err(format!("skill bundle file exceeds {MAX_FILE_BYTES} bytes"));
        }
        total = total.saturating_add(content.len());
        if total > MAX_BYTES {
            return Err(format!("skill bundle exceeds {MAX_BYTES} bytes"));
        }
        if bundle.insert(path.clone(), content).is_some() {
            return Err(format!("duplicate skill bundle path `{path}`"));
        }
    }
    Ok(bundle)
}

fn bundle_skill_md(bundle: &BTreeMap<String, Vec<u8>>) -> Result<&str, String> {
    bundle
        .iter()
        .find(|(name, _)| *name == "SKILL.md" || name.ends_with("/SKILL.md"))
        .ok_or_else(|| "skill upload has no SKILL.md file".to_string())
        .and_then(|(_, content)| {
            std::str::from_utf8(content).map_err(|_| "SKILL.md must be valid UTF-8".to_string())
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
    files: BTreeMap<String, Vec<u8>>,
) -> SkillVersion {
    let spec = awaken_ext_skills::parse_skill_md("skill", content);
    let files = files
        .into_iter()
        .map(|(path, content)| SkillBundleFile { path, content })
        .collect::<Vec<_>>();
    SkillVersion {
        id: format!(
            "skver_{}_{ordinal}",
            awaken_skill_store::sanitize_stem(skill_id)
        )
        .into(),
        skill_id: skill_id.into(),
        version: ordinal,
        name: spec.name.clone(),
        description: spec.description.clone(),
        directory: format!("/skills/{}", awaken_skill_store::sanitize_stem(&spec.name)),
        bundle_sha256: awaken_skill_store::bundle_sha256(&files),
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

/// `POST /v1/skills` — create a skill. Multipart (SDK) uploads a SKILL.md (+
/// supporting files); a JSON body `{id, content}` keeps the legacy delivery
/// contract. Both register a v1 and feed the runtime catalog.
async fn create_skill(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    headers: HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.starts_with("multipart/form-data") {
        // SDK path: rebuild a request carrying the multipart headers + body so the
        // `Multipart` extractor can parse the boundary.
        let mut builder = axum::http::Request::builder();
        if let Some(h) = builder.headers_mut() {
            *h = headers;
        }
        let req = match builder.body(body) {
            Ok(r) => r,
            Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
        };
        let multipart = match Multipart::from_request(req, &()).await {
            Ok(m) => m,
            Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
        };
        let (display_title, files) = read_multipart(multipart).await;
        let bundle = match normalize_bundle(files) {
            Ok(bundle) => bundle,
            Err(error) => return err(StatusCode::BAD_REQUEST, error),
        };
        let content = match bundle_skill_md(&bundle) {
            Ok(content) => content.to_string(),
            Err(error) => return err(StatusCode::BAD_REQUEST, error),
        };
        let parsed = awaken_ext_skills::parse_skill_md("skill", &content);
        let id = awaken_skill_store::catalog_id(&parsed.name);
        let version = build_version(&id, &content, 1, bundle);
        let definition = SkillDefinition {
            id: id.clone().into(),
            workspace_id: workspace.clone().into(),
            display_title,
            latest_version: 1,
            last_version: 1,
            timestamps: awaken_protocol_managed::resource_plane::ResourceTimestamps::created(
                version.created_unix_nanos,
            ),
        };
        let Some(result) = state.create(definition.clone(), version).await else {
            return err(
                StatusCode::CONFLICT,
                "this server has no durable skill store",
            );
        };
        return match result {
            Ok(()) => (StatusCode::OK, Json(project_definition(&definition))).into_response(),
            Err(error) => store_error(error),
        };
    }

    // Legacy JSON path: `{id, content}` → durable delivery + a registry entry so
    // the SDK list/retrieve/delete stay consistent.
    let bytes = match axum::body::to_bytes(body, 2 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let (Some(id), Some(content)) = (
        json.get("id").and_then(Value::as_str),
        json.get("content").and_then(Value::as_str),
    ) else {
        return err(
            StatusCode::BAD_REQUEST,
            "skill needs a string `id` and `content`",
        );
    };
    let id = awaken_skill_store::sanitize_stem(id);
    let version = build_version(
        &id,
        content,
        1,
        BTreeMap::from([("SKILL.md".to_owned(), content.as_bytes().to_vec())]),
    );
    let definition = SkillDefinition {
        id: id.clone().into(),
        workspace_id: workspace.into(),
        display_title: None,
        latest_version: 1,
        last_version: 1,
        timestamps: awaken_protocol_managed::resource_plane::ResourceTimestamps::created(
            version.created_unix_nanos,
        ),
    };
    let Some(result) = state.create(definition, version).await else {
        return err(
            StatusCode::CONFLICT,
            "this server has no durable skill store",
        );
    };
    match result {
        Ok(()) => (StatusCode::OK, Json(json!({ "id": id, "type": "skill" }))).into_response(),
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
) -> axum::response::Response {
    let definitions = match state.definitions(&workspace).await {
        Ok(definitions) => definitions,
        Err(error) => return store_error(error),
    };
    let data = definitions
        .iter()
        .map(project_definition)
        .collect::<Vec<_>>();
    (
        StatusCode::OK,
        Json(json!({ "data": data, "has_more": false, "next_page": null })),
    )
        .into_response()
}

async fn retrieve_skill(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> axum::response::Response {
    match state.definition(&workspace, &id).await {
        Some(Ok(Some(definition))) => {
            (StatusCode::OK, Json(project_definition(&definition))).into_response()
        }
        Some(Err(error)) => store_error(error),
        None => err(StatusCode::NOT_FOUND, format!("skill `{id}` not found")),
        Some(Ok(None)) => err(StatusCode::NOT_FOUND, format!("skill `{id}` not found")),
    }
}

async fn delete_skill(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> axum::response::Response {
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
            Json(json!({ "id": id, "type": "skill_deleted" })),
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
    multipart: Multipart,
) -> axum::response::Response {
    let definition = match state.definition(&workspace, &id).await {
        Some(Ok(Some(definition))) => definition,
        Some(Err(error)) => return store_error(error),
        Some(Ok(None)) | None => {
            return err(StatusCode::NOT_FOUND, format!("skill `{id}` not found"));
        }
    };
    let (_title, files) = read_multipart(multipart).await;
    let bundle = match normalize_bundle(files) {
        Ok(bundle) => bundle,
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    let content = match bundle_skill_md(&bundle) {
        Ok(content) => content.to_string(),
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    let version = build_version(&id, &content, definition.last_version + 1, bundle);
    let projected = project_version(&version);
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
) -> axum::response::Response {
    match state.versions(&workspace, &id).await {
        Some(Ok(versions)) if !versions.is_empty() => {
            let data: Vec<Value> = versions.iter().map(project_version).collect();
            (
                StatusCode::OK,
                Json(json!({ "data": data, "has_more": false, "next_page": null })),
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
) -> axum::response::Response {
    match find_version(&state, &workspace, &id, &version).await {
        Ok(Some(version)) => (StatusCode::OK, Json(project_version(&version))).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "skill version not found"),
        Err(error) => store_error(error),
    }
}

async fn delete_version(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, version)): Path<(String, String)>,
) -> axum::response::Response {
    let found = match find_version(&state, &workspace, &id, &version).await {
        Ok(Some(version)) => version,
        Ok(None) => return err(StatusCode::NOT_FOUND, "skill version not found"),
        Err(error) => return store_error(error),
    };
    match state.delete_version(&workspace, &id, found.version).await {
        Some(Ok(true)) => (
            StatusCode::OK,
            Json(json!({ "id": found.id, "type": "skill_version_deleted" })),
        )
            .into_response(),
        Some(Ok(false)) | None => err(StatusCode::NOT_FOUND, "skill version not found"),
        Some(Err(error)) => store_error(error),
    }
}

/// `GET /v1/skills/:id/versions/:version/content` — the version's raw SKILL.md.
async fn version_content(
    State(state): State<Arc<SkillsApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path((id, version)): Path<(String, String)>,
) -> axum::response::Response {
    match find_version(&state, &workspace, &id, &version).await {
        Ok(Some(version)) => match version.skill_md() {
            Some(content) => (StatusCode::OK, content.to_vec()).into_response(),
            None => err(StatusCode::NOT_FOUND, "skill version content not found"),
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
) -> axum::response::Response {
    let normalized = match normalize_bundle(vec![(path, Vec::new())]) {
        Ok(bundle) => bundle.into_keys().next().expect("one normalized path"),
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
    use crate::SharedHost;
    use awaken_runtime_contract::llm::{ChatRequest, ChatResponse};
    use awaken_tenancy::WorkspaceScope;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    struct NoLlm;
    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            unreachable!("the skills API never calls the model")
        }
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

    #[tokio::test]
    async fn the_same_skill_id_is_isolated_by_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(
            SharedHost::new(Arc::new(NoLlm), "test").with_skill_store(dir.path().join("skills")),
        );
        host.skills
            .persist_authored(
                "ws_a",
                "private",
                "---\nname: private\ndescription: a\n---\nsecret-a",
            )
            .await;
        let router = skills_router(host.skill_store(), host);
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

    // A skill delivered to the durable store OUTSIDE the SDK `/v1/skills` create
    // route (harvested authored skills, or any skill after a restart clears the
    // in-memory registry) must still resolve by its advertised catalog id — the
    // A-1 fallback the happy-path e2e (which always creates via `/v1/skills`) never
    // exercises.
    #[tokio::test]
    async fn a_durable_only_skill_resolves_by_its_catalog_id() {
        let dir = std::env::temp_dir().join(format!("awaken-skillsapi-{}", std::process::id()));
        let host = std::sync::Arc::new(
            SharedHost::new(std::sync::Arc::new(NoLlm), "test").with_skill_store(dir.join("store")),
        );
        let workspace = host.local_workspace().to_string();
        // Deliver straight to the durable repository — the API reads the same truth.
        host.skills
            .persist_authored(
                &workspace,
                "Greeter",
                "---\nname: Greeter\ndescription: hi\n---\nsay hello",
            )
            .await;
        let router = skills_router(host.skill_store(), host);
        let cid = "Greeter";

        // The advertised catalog id retrieves the skill via the durable fallback…
        let (status, _) = get_in(&router, &format!("/v1/skills/{cid}"), &workspace).await;
        assert_eq!(status, StatusCode::OK, "catalog id retrieves the skill");
        // …and `version: "latest"` downloads its content (what the worker fetches).
        let (status, body) = get_in(
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

        let _ = std::fs::remove_dir_all(&dir);
    }
}
