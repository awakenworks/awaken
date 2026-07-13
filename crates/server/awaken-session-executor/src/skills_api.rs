//! The skills API (`/v1/skills`) over the host's durable delivered-skill catalog
//! (ADR-0036) plus the official `@anthropic-ai/sdk` `beta.skills.*` surface:
//! multipart create, retrieve, list, delete, and the `versions` subresource
//! (create / retrieve / list / delete / get-content).
//!
//! Two create paths coexist so nothing regresses: a JSON body `{id, content}`
//! keeps the original delivery contract (used by the resource-mount e2e), while a
//! multipart upload is the SDK path. BOTH feed the runtime's delivered-skill
//! catalog ([`awaken_skill_store::SkillStore`]) so a skill created either way is
//! offered on every thread and survives a restart; the SDK's richer object +
//! version history lives in an in-memory registry keyed by skill id.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{FromRequest, Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::host::SharedHost;

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// One stored version of a skill (the SKILL.md content + its projected metadata).
#[derive(Clone)]
struct SkillVersion {
    id: String,
    version: String,
    name: String,
    description: String,
    directory: String,
    content: String,
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
        })
    }
}

/// A stored skill: its display metadata + ordered version history.
#[derive(Clone)]
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
    registry: Mutex<BTreeMap<String, SkillRecord>>,
    skill_seq: AtomicU64,
    version_seq: AtomicU64,
}

/// Mount the skills API over the host's durable skill catalog.
pub fn skills_router(host: Arc<SharedHost>) -> Router {
    let state = Arc::new(SkillsApi {
        host,
        registry: Mutex::new(BTreeMap::new()),
        skill_seq: AtomicU64::new(0),
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
        .with_state(state)
}

fn err(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
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

/// Pick the SKILL.md among the uploaded files (exact name, else any `.md`, else
/// the first file), returning its content.
fn pick_skill_md(files: &[(String, String)]) -> Option<&str> {
    files
        .iter()
        .find(|(n, _)| n.ends_with("SKILL.md"))
        .or_else(|| files.iter().find(|(n, _)| n.ends_with(".md")))
        .or_else(|| files.first())
        .map(|(_, c)| c.as_str())
}

/// Build a version's projected metadata from SKILL.md content (parse frontmatter
/// for name + description). Delivery to the durable catalog is the caller's job —
/// the SDK path delivers under the skill's name, the legacy path under its id — so
/// a skill is stored under exactly one durable id (no double-write).
fn build_version(state: &SkillsApi, content: &str) -> SkillVersion {
    let spec = awaken_ext_skills::parse_skill_md("skill", content);
    let n = state.version_seq.fetch_add(1, Ordering::SeqCst);
    SkillVersion {
        id: format!("skver_{n:016}"),
        version: (n + 1).to_string(),
        name: spec.name.clone(),
        description: spec.description.clone(),
        directory: format!("/skills/{}", awaken_skill_store::sanitize_stem(&spec.name)),
        content: content.to_string(),
    }
}

// ---- Skill routes ----------------------------------------------------------

/// `POST /v1/skills` — create a skill. Multipart (SDK) uploads a SKILL.md (+
/// supporting files); a JSON body `{id, content}` keeps the legacy delivery
/// contract. Both register a v1 and feed the runtime catalog.
async fn create_skill(
    State(state): State<Arc<SkillsApi>>,
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
        let Some(content) = pick_skill_md(&files).map(str::to_string) else {
            return err(StatusCode::BAD_REQUEST, "skill upload has no SKILL.md file");
        };
        let version = build_version(&state, &content);
        // Deliver under the skill's name so the runtime offers it on threads.
        let _ = state.host.skill_store_put(&version.name, &content).await;
        let n = state.skill_seq.fetch_add(1, Ordering::SeqCst);
        let id = format!("skill_{n:016}");
        let record = SkillRecord {
            display_title,
            versions: vec![version],
        };
        let projected = record.project(&id);
        state.registry.lock().unwrap().insert(id, record);
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
    match state.host.skill_store_put(id, content).await {
        Some(stored_id) => {
            let version = build_version(&state, content);
            state.registry.lock().unwrap().insert(
                stored_id.clone(),
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
async fn list_skills(State(state): State<Arc<SkillsApi>>) -> impl IntoResponse {
    // Snapshot the registry (and its ids) then drop the lock before awaiting the
    // durable list — a MutexGuard must not be held across an await.
    let (mut data, present): (Vec<Value>, std::collections::HashSet<String>) = {
        let registry = state.registry.lock().unwrap();
        (
            registry.iter().map(|(id, r)| r.project(id)).collect(),
            registry.keys().cloned().collect(),
        )
    };
    for id in state.host.skill_store_list().await {
        if !present.contains(&id) {
            data.push(json!({
                "id": id,
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
    Path(id): Path<String>,
) -> axum::response::Response {
    let registry = state.registry.lock().unwrap();
    match registry.get(&id) {
        Some(r) => (StatusCode::OK, Json(r.project(&id))).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("skill `{id}` not found")),
    }
}

async fn delete_skill(
    State(state): State<Arc<SkillsApi>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let removed = state.registry.lock().unwrap().remove(&id).is_some();
    if removed {
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
    Path(id): Path<String>,
    multipart: Multipart,
) -> axum::response::Response {
    {
        let registry = state.registry.lock().unwrap();
        if !registry.contains_key(&id) {
            return err(StatusCode::NOT_FOUND, format!("skill `{id}` not found"));
        }
    }
    let (_title, files) = read_multipart(multipart).await;
    let Some(content) = pick_skill_md(&files).map(str::to_string) else {
        return err(
            StatusCode::BAD_REQUEST,
            "version upload has no SKILL.md file",
        );
    };
    let version = build_version(&state, &content);
    // Deliver the new version's content under the skill's name.
    let _ = state.host.skill_store_put(&version.name, &content).await;
    let projected = version.project(&id);
    let mut registry = state.registry.lock().unwrap();
    registry.get_mut(&id).unwrap().versions.push(version);
    (StatusCode::OK, Json(projected)).into_response()
}

async fn list_versions(
    State(state): State<Arc<SkillsApi>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let registry = state.registry.lock().unwrap();
    match registry.get(&id) {
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
    record
        .versions
        .iter()
        .find(|v| v.version == version || v.id == version)
}

async fn retrieve_version(
    State(state): State<Arc<SkillsApi>>,
    Path((id, version)): Path<(String, String)>,
) -> axum::response::Response {
    let registry = state.registry.lock().unwrap();
    match registry.get(&id).and_then(|r| find_version(r, &version)) {
        Some(v) => (StatusCode::OK, Json(v.project(&id))).into_response(),
        None => err(StatusCode::NOT_FOUND, "skill version not found"),
    }
}

async fn delete_version(
    State(state): State<Arc<SkillsApi>>,
    Path((id, version)): Path<(String, String)>,
) -> axum::response::Response {
    let mut registry = state.registry.lock().unwrap();
    let Some(record) = registry.get_mut(&id) else {
        return err(StatusCode::NOT_FOUND, format!("skill `{id}` not found"));
    };
    let before = record.versions.len();
    let deleted_id = find_version(record, &version).map(|v| v.id.clone());
    record
        .versions
        .retain(|v| v.version != version && v.id != version);
    match deleted_id {
        Some(vid) if record.versions.len() < before => (
            StatusCode::OK,
            Json(json!({ "id": vid, "type": "skill_version_deleted" })),
        )
            .into_response(),
        _ => err(StatusCode::NOT_FOUND, "skill version not found"),
    }
}

/// `GET /v1/skills/:id/versions/:version/content` — the version's raw SKILL.md.
async fn version_content(
    State(state): State<Arc<SkillsApi>>,
    Path((id, version)): Path<(String, String)>,
) -> axum::response::Response {
    let registry = state.registry.lock().unwrap();
    match registry.get(&id).and_then(|r| find_version(r, &version)) {
        Some(v) => (StatusCode::OK, v.content.clone()).into_response(),
        None => err(StatusCode::NOT_FOUND, "skill version not found"),
    }
}
