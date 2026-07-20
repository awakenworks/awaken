//! The skills API (`/v1/skills`) over the host's durable delivered-skill catalog
//! (ADR-0036) plus the official `@anthropic-ai/sdk` `beta.skills.*` surface:
//! multipart create, retrieve, list, delete, and the `versions` subresource
//! (create / retrieve / list / delete / get-content).
//!
//! Two create paths coexist so nothing regresses: a JSON body `{id, content}`
//! keeps the original delivery contract (used by the resource-mount e2e), while a
//! multipart upload is the SDK path. BOTH feed the runtime's delivered-skill
//! catalog ([`awaken_skill_store::SkillStore`]) so a skill created either way is
//! offered on every thread and survives a restart. The SDK's richer object and
//! version history share a workspace-scoped SQLite projection in durable mode;
//! only an explicitly ephemeral host uses the in-memory adapter.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use awaken_tenancy::WorkspaceScope;
use axum::extract::{Extension, FromRequest, Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::host::SharedHost;

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// One stored version of a skill (the SKILL.md content + its projected metadata).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SkillVersion {
    id: String,
    version: String,
    name: String,
    description: String,
    directory: String,
    content: String,
    /// Complete uploaded bundle keyed by normalized relative path. Older rows
    /// deserialize as an empty map and retain their SKILL.md through `content`.
    #[serde(default)]
    files: BTreeMap<String, String>,
}

impl SkillVersion {
    fn project(&self, skill_id: &str) -> Value {
        json!({
            "id": self.id,
            "type": "skill_version",
            "created_at": OBJECT_AT,
            "description": self.description,
            "directory": self.directory,
            "name": self.name,
            "skill_id": skill_id,
            "version": self.version,
            "files": self.files.keys().collect::<Vec<_>>(),
        })
    }
}

/// A stored skill: its display metadata + ordered version history.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SkillRecord {
    display_title: Option<String>,
    versions: Vec<SkillVersion>,
}

impl SkillRecord {
    fn project(&self, id: &str) -> Value {
        json!({
            "id": id,
            "type": "skill",
            "created_at": OBJECT_AT,
            "updated_at": OBJECT_AT,
            "display_title": self.display_title,
            "latest_version": self.versions.last().map(|v| v.version.clone()),
            "source": "api",
        })
    }
}

/// The skills API state: the runtime host (for delivery) + the SDK registry.
struct SkillsApi {
    host: Arc<SharedHost>,
    registry: SkillRegistry,
    version_seq: AtomicU64,
}

enum SkillRegistry {
    Memory(Mutex<BTreeMap<(String, String), SkillRecord>>),
    Sqlite(Mutex<rusqlite::Connection>),
}

impl SkillRegistry {
    fn open() -> Self {
        let Some(dir) = std::env::var("AWAKEN_STORAGE_DIR")
            .ok()
            .filter(|dir| !dir.trim().is_empty())
            .map(std::path::PathBuf::from)
        else {
            return Self::Memory(Mutex::new(BTreeMap::new()));
        };
        Self::open_at(&dir)
    }

    fn open_at(dir: &std::path::Path) -> Self {
        std::fs::create_dir_all(dir).expect("create resource API storage directory");
        let conn = rusqlite::Connection::open(dir.join("resource-api.db"))
            .expect("open resource API database");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS skill_records (\
                 workspace_id TEXT NOT NULL,\
                 skill_id TEXT NOT NULL,\
                 data TEXT NOT NULL,\
                 PRIMARY KEY(workspace_id, skill_id)\
             );",
        )
        .expect("migrate skill registry");
        Self::Sqlite(Mutex::new(conn))
    }

    fn get(&self, workspace: &str, id: &str) -> Option<SkillRecord> {
        match self {
            Self::Memory(records) => records
                .lock()
                .expect("skill registry")
                .get(&(workspace.to_string(), id.to_string()))
                .cloned(),
            Self::Sqlite(conn) => conn
                .lock()
                .expect("skill registry")
                .query_row(
                    "SELECT data FROM skill_records WHERE workspace_id = ?1 AND skill_id = ?2",
                    rusqlite::params![workspace, id],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .map(|data| serde_json::from_str(&data).expect("decode skill record")),
        }
    }

    fn put(&self, workspace: &str, id: &str, record: SkillRecord) {
        match self {
            Self::Memory(records) => {
                records
                    .lock()
                    .expect("skill registry")
                    .insert((workspace.to_string(), id.to_string()), record);
            }
            Self::Sqlite(conn) => {
                let data = serde_json::to_string(&record).expect("encode skill record");
                conn.lock()
                    .expect("skill registry")
                    .execute(
                        "INSERT INTO skill_records(workspace_id, skill_id, data) VALUES (?1, ?2, ?3) \
                         ON CONFLICT(workspace_id, skill_id) DO UPDATE SET data = excluded.data",
                        rusqlite::params![workspace, id, data],
                    )
                    .expect("persist skill record");
            }
        }
    }

    fn remove(&self, workspace: &str, id: &str) -> bool {
        match self {
            Self::Memory(records) => records
                .lock()
                .expect("skill registry")
                .remove(&(workspace.to_string(), id.to_string()))
                .is_some(),
            Self::Sqlite(conn) => {
                conn.lock()
                    .expect("skill registry")
                    .execute(
                        "DELETE FROM skill_records WHERE workspace_id = ?1 AND skill_id = ?2",
                        rusqlite::params![workspace, id],
                    )
                    .expect("delete skill record")
                    > 0
            }
        }
    }

    fn list(&self, workspace: &str) -> Vec<(String, SkillRecord)> {
        match self {
            Self::Memory(records) => records
                .lock()
                .expect("skill registry")
                .iter()
                .filter(|((owner, _), _)| owner == workspace)
                .map(|((_, id), record)| (id.clone(), record.clone()))
                .collect(),
            Self::Sqlite(conn) => {
                let conn = conn.lock().expect("skill registry");
                let mut stmt = conn
                    .prepare(
                        "SELECT skill_id, data FROM skill_records WHERE workspace_id = ?1 ORDER BY skill_id",
                    )
                    .expect("prepare skill registry list");
                stmt.query_map(rusqlite::params![workspace], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .expect("list skill registry")
                .map(|row| {
                    let (id, data) = row.expect("read skill record");
                    (
                        id,
                        serde_json::from_str(&data).expect("decode skill record"),
                    )
                })
                .collect()
            }
        }
    }
}

/// Mount the skills API over the host's durable skill catalog.
pub fn skills_router(host: Arc<SharedHost>) -> Router {
    let state = Arc::new(SkillsApi {
        host,
        registry: SkillRegistry::open(),
        version_seq: AtomicU64::new(0),
    });
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

fn request_workspace(state: &SkillsApi, scope: Option<Extension<WorkspaceScope>>) -> String {
    scope.map_or_else(
        || state.host.local_workspace().to_string(),
        |Extension(scope)| scope.0,
    )
}

/// Collect a multipart body into `(display_title, files)` where each file is
/// `(filename, utf8-content)`. Non-file text fields are read for `display_title`.
async fn read_multipart(mut multipart: Multipart) -> (Option<String>, Vec<(String, String)>) {
    let mut display_title = None;
    let mut files = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().map(str::to_string);
        let filename = field.file_name().map(str::to_string);
        if let Some(fname) = filename {
            if let Ok(bytes) = field.bytes().await {
                files.push((fname, String::from_utf8_lossy(&bytes).to_string()));
            }
        } else if name.as_deref() == Some("display_title") {
            display_title = field.text().await.ok();
        }
    }
    (display_title, files)
}

fn normalize_bundle(files: Vec<(String, String)>) -> Result<BTreeMap<String, String>, String> {
    const MAX_FILES: usize = 128;
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

fn bundle_skill_md(bundle: &BTreeMap<String, String>) -> Option<&str> {
    bundle
        .iter()
        .find(|(name, _)| name.ends_with("SKILL.md"))
        .or_else(|| bundle.iter().find(|(name, _)| name.ends_with(".md")))
        .or_else(|| bundle.iter().next())
        .map(|(_, content)| content.as_str())
}

/// Build a version's projected metadata from SKILL.md content (parse frontmatter
/// for name + description). Delivery to the durable catalog is the caller's job —
/// the SDK path delivers under the skill's name, the legacy path under its id — so
/// a skill is stored under exactly one durable id (no double-write).
fn build_version(
    state: &SkillsApi,
    content: &str,
    ordinal: usize,
    files: BTreeMap<String, String>,
) -> SkillVersion {
    let spec = awaken_ext_skills::parse_skill_md("skill", content);
    let n = state.version_seq.fetch_add(1, Ordering::SeqCst);
    SkillVersion {
        id: format!("skver_{n:016}"),
        version: ordinal.to_string(),
        name: spec.name.clone(),
        description: spec.description.clone(),
        directory: format!("/skills/{}", awaken_skill_store::sanitize_stem(&spec.name)),
        content: content.to_string(),
        files,
    }
}

// ---- Skill routes ----------------------------------------------------------

/// `POST /v1/skills` — create a skill. Multipart (SDK) uploads a SKILL.md (+
/// supporting files); a JSON body `{id, content}` keeps the legacy delivery
/// contract. Both register a v1 and feed the runtime catalog.
async fn create_skill(
    State(state): State<Arc<SkillsApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    let workspace = request_workspace(&state, scope);
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
        let Some(content) = bundle_skill_md(&bundle).map(str::to_string) else {
            return err(StatusCode::BAD_REQUEST, "skill upload has no SKILL.md file");
        };
        let version = build_version(&state, &content, 1, bundle);
        // Deliver under the skill's name so the runtime offers it on threads. Fail
        // closed (409) when no durable store is wired — exactly like the legacy JSON
        // path below — rather than returning 200 with only an ephemeral registry
        // entry (a skill that was neither delivered nor persisted).
        if state
            .host
            .skills
            .store_put_in(&workspace, &version.name, &content)
            .await
            .is_none()
        {
            return err(
                StatusCode::CONFLICT,
                "this server has no durable skill store",
            );
        }
        // Register (and return) the tagged catalog id — the same id advertisement
        // derives from the name — so the official worker downloads what it was told.
        let id = awaken_skill_store::catalog_id(&version.name);
        let record = SkillRecord {
            display_title,
            versions: vec![version],
        };
        let projected = record.project(&id);
        state.registry.put(&workspace, &id, record);
        return (StatusCode::OK, Json(projected)).into_response();
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
    match state
        .host
        .skills
        .store_put_in(&workspace, id, content)
        .await
    {
        Some(stored_id) => {
            let version = build_version(
                &state,
                content,
                1,
                BTreeMap::from([("SKILL.md".to_owned(), content.to_owned())]),
            );
            state.registry.put(
                &workspace,
                &stored_id,
                SkillRecord {
                    display_title: None,
                    versions: vec![version],
                },
            );
            (
                StatusCode::OK,
                Json(json!({ "id": stored_id, "type": "skill" })),
            )
                .into_response()
        }
        None => err(
            StatusCode::CONFLICT,
            "this server has no durable skill store",
        ),
    }
}

/// `GET /v1/skills` — the registered skills as a cursor page. Includes durable
/// delivered skills the in-memory registry does not track (e.g. after a restart,
/// when the registry is empty but the durable catalog persists), so a skill
/// uploaded before a restart still lists.
async fn list_skills(
    State(state): State<Arc<SkillsApi>>,
    scope: Option<Extension<WorkspaceScope>>,
) -> impl IntoResponse {
    let workspace = request_workspace(&state, scope);
    // Snapshot the registry (and its ids) then drop the lock before awaiting the
    // durable list — a MutexGuard must not be held across an await.
    let (mut data, present): (Vec<Value>, std::collections::HashSet<String>) = {
        let registry = state.registry.list(&workspace);
        (
            registry.iter().map(|(id, r)| r.project(id)).collect(),
            registry.iter().map(|(id, _)| id.clone()).collect(),
        )
    };
    for stem in state.host.skills.store_list_in(&workspace).await {
        let cid = awaken_skill_store::catalog_id(&stem);
        // Skip if already surfaced by the registry under either its catalog id (SDK
        // path) or its raw stem (legacy `{id, content}` path). Otherwise list under
        // the durable STEM — the stable id a legacy-delivered skill keeps across a
        // restart. Advertisement (`skill_ids`) is what offers the tagged catalog id
        // to the worker; the read paths resolve either form, so listing the stem
        // here keeps the durability contract without breaking the worker download.
        if !present.contains(&cid) && !present.contains(&stem) {
            data.push(json!({
                "id": stem,
                "type": "skill",
                "created_at": OBJECT_AT,
                "updated_at": OBJECT_AT,
                "display_title": null,
                "latest_version": null,
                "source": "api",
            }));
        }
    }
    (
        StatusCode::OK,
        Json(json!({ "data": data, "has_more": false, "next_page": null })),
    )
}

async fn retrieve_skill(
    State(state): State<Arc<SkillsApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let workspace = request_workspace(&state, scope);
    match resolve_record(&state, &workspace, &id).await {
        Some(r) => (StatusCode::OK, Json(r.project(&id))).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("skill `{id}` not found")),
    }
}

async fn delete_skill(
    State(state): State<Arc<SkillsApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let workspace = request_workspace(&state, scope);
    // Remove the richer process-local projection first, then the durable source of
    // truth. The lock must not cross the async store call. Previously only the
    // projection was removed, so `retrieve` immediately resurrected the skill from
    // the durable catalog and the delete receipt was false.
    let removed_projection = state.registry.remove(&workspace, &id);
    let removed_durable = state
        .host
        .skills
        .store_delete_in(&workspace, &id)
        .await
        .unwrap_or(false);
    if removed_projection || removed_durable {
        (
            StatusCode::OK,
            Json(json!({ "id": id, "type": "skill_deleted" })),
        )
            .into_response()
    } else {
        err(StatusCode::NOT_FOUND, format!("skill `{id}` not found"))
    }
}

// ---- Version routes --------------------------------------------------------

async fn create_version(
    State(state): State<Arc<SkillsApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
    multipart: Multipart,
) -> axum::response::Response {
    let workspace = request_workspace(&state, scope);
    let Some(mut record) = state.registry.get(&workspace, &id) else {
        return err(StatusCode::NOT_FOUND, format!("skill `{id}` not found"));
    };
    let (_title, files) = read_multipart(multipart).await;
    let bundle = match normalize_bundle(files) {
        Ok(bundle) => bundle,
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    let Some(content) = bundle_skill_md(&bundle).map(str::to_string) else {
        return err(
            StatusCode::BAD_REQUEST,
            "version upload has no SKILL.md file",
        );
    };
    let version = build_version(&state, &content, record.versions.len() + 1, bundle);
    // Deliver the new version's content under the skill's name.
    let _ = state
        .host
        .skills
        .store_put_in(&workspace, &version.name, &content)
        .await;
    let projected = version.project(&id);
    record.versions.push(version);
    state.registry.put(&workspace, &id, record);
    (StatusCode::OK, Json(projected)).into_response()
}

async fn list_versions(
    State(state): State<Arc<SkillsApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let workspace = request_workspace(&state, scope);
    match state.registry.get(&workspace, &id) {
        Some(r) => {
            let data: Vec<Value> = r.versions.iter().map(|v| v.project(&id)).collect();
            (
                StatusCode::OK,
                Json(json!({ "data": data, "has_more": false, "next_page": null })),
            )
                .into_response()
        }
        None => err(StatusCode::NOT_FOUND, format!("skill `{id}` not found")),
    }
}

fn find_version<'a>(record: &'a SkillRecord, version: &str) -> Option<&'a SkillVersion> {
    if version == "latest" {
        // The advertisement pins `version: "latest"`, so the worker downloads by it.
        return record.versions.last();
    }
    record
        .versions
        .iter()
        .find(|v| v.version == version || v.id == version)
}

/// Resolve a skill record by id: the in-memory registry first, then the durable
/// catalog (so an advertised catalog id resolves even for a skill delivered outside
/// the SDK create route, or after a restart cleared the registry). Synthesizes a
/// single-version record from the durable content.
async fn resolve_record(state: &SkillsApi, workspace: &str, id: &str) -> Option<SkillRecord> {
    if let Some(r) = state.registry.get(workspace, id) {
        return Some(r);
    }
    let (_stem, content) = state.host.skills.by_catalog_id_in(workspace, id)?;
    Some(SkillRecord {
        display_title: None,
        versions: vec![build_version(
            state,
            &content,
            1,
            BTreeMap::from([("SKILL.md".to_owned(), content.clone())]),
        )],
    })
}

async fn retrieve_version(
    State(state): State<Arc<SkillsApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((id, version)): Path<(String, String)>,
) -> axum::response::Response {
    let workspace = request_workspace(&state, scope);
    match resolve_record(&state, &workspace, &id)
        .await
        .as_ref()
        .and_then(|r| find_version(r, &version).map(|v| v.project(&id)))
    {
        Some(projected) => (StatusCode::OK, Json(projected)).into_response(),
        None => err(StatusCode::NOT_FOUND, "skill version not found"),
    }
}

async fn delete_version(
    State(state): State<Arc<SkillsApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((id, version)): Path<(String, String)>,
) -> axum::response::Response {
    let workspace = request_workspace(&state, scope);
    let Some(mut record) = state.registry.get(&workspace, &id) else {
        return err(StatusCode::NOT_FOUND, format!("skill `{id}` not found"));
    };
    let before = record.versions.len();
    let deleted_id = find_version(&record, &version).map(|v| v.id.clone());
    record
        .versions
        .retain(|v| v.version != version && v.id != version);
    match deleted_id {
        Some(vid) if record.versions.len() < before => {
            state.registry.put(&workspace, &id, record);
            (
                StatusCode::OK,
                Json(json!({ "id": vid, "type": "skill_version_deleted" })),
            )
                .into_response()
        }
        _ => err(StatusCode::NOT_FOUND, "skill version not found"),
    }
}

/// `GET /v1/skills/:id/versions/:version/content` — the version's raw SKILL.md.
async fn version_content(
    State(state): State<Arc<SkillsApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((id, version)): Path<(String, String)>,
) -> axum::response::Response {
    let workspace = request_workspace(&state, scope);
    match resolve_record(&state, &workspace, &id)
        .await
        .as_ref()
        .and_then(|r| find_version(r, &version).map(|v| v.content.clone()))
    {
        Some(content) => (StatusCode::OK, content).into_response(),
        None => err(StatusCode::NOT_FOUND, "skill version not found"),
    }
}

/// Retrieve one support file from the immutable uploaded bundle. Traversal and
/// absolute paths are rejected by the same normalization used at ingestion.
async fn version_file(
    State(state): State<Arc<SkillsApi>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((id, version, path)): Path<(String, String, String)>,
) -> axum::response::Response {
    let workspace = request_workspace(&state, scope);
    let normalized = match normalize_bundle(vec![(path, String::new())]) {
        Ok(bundle) => bundle.into_keys().next().expect("one normalized path"),
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    match resolve_record(&state, &workspace, &id)
        .await
        .as_ref()
        .and_then(|record| find_version(record, &version))
        .and_then(|version| version.files.get(&normalized).cloned())
    {
        Some(content) => (StatusCode::OK, content).into_response(),
        None => err(StatusCode::NOT_FOUND, "skill version file not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::{ChatRequest, ChatResponse};
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
        let resp = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
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

    #[test]
    fn rich_skill_versions_survive_registry_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::open_at(dir.path());
        registry.put(
            "ws_a",
            "skill_a",
            SkillRecord {
                display_title: Some("A".into()),
                versions: vec![SkillVersion {
                    id: "skver_1".into(),
                    version: "1".into(),
                    name: "a".into(),
                    description: "first".into(),
                    directory: "/skills/a".into(),
                    content: "# first".into(),
                    files: BTreeMap::from([
                        ("SKILL.md".into(), "# first".into()),
                        ("references/guide.md".into(), "guide".into()),
                    ]),
                }],
            },
        );
        drop(registry);

        let reopened = SkillRegistry::open_at(dir.path());
        let record = reopened.get("ws_a", "skill_a").unwrap();
        assert_eq!(record.display_title.as_deref(), Some("A"));
        assert_eq!(record.versions[0].content, "# first");
        assert_eq!(
            record.versions[0].files.get("references/guide.md"),
            Some(&"guide".to_string())
        );
        assert!(reopened.get("ws_b", "skill_a").is_none());
    }

    #[tokio::test]
    async fn the_same_skill_id_is_isolated_by_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(
            SharedHost::new(Arc::new(NoLlm), "test").with_skill_store(dir.path().join("skills")),
        );
        host.skills
            .store_put_in(
                "ws_a",
                "private",
                "---\nname: private\ndescription: a\n---\nsecret-a",
            )
            .await;
        let router = skills_router(host);
        let id = awaken_skill_store::catalog_id("private");
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
        // Deliver straight to the durable catalog — no registry entry.
        host.skills
            .store_put(
                "Greeter",
                "---\nname: Greeter\ndescription: hi\n---\nsay hello",
            )
            .await;
        let router = skills_router(host);
        let cid = awaken_skill_store::catalog_id("Greeter");

        // The advertised catalog id retrieves the skill via the durable fallback…
        let (status, _) = get(&router, &format!("/v1/skills/{cid}")).await;
        assert_eq!(status, StatusCode::OK, "catalog id retrieves the skill");
        // …and `version: "latest"` downloads its content (what the worker fetches).
        let (status, body) = get(
            &router,
            &format!("/v1/skills/{cid}/versions/latest/content"),
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
        let (_, list) = get(&router, "/v1/skills").await;
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
