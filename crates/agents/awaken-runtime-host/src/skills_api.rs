//! The skills API (`/v1/skills`) over the host's durable delivered-skill catalog
//! (ADR-0036). A managed client uploads a `SKILL.md` under an id; the host persists
//! it in the resources-plane [`awaken_skill_store::SkillStore`], so the skill is
//! offered on every thread (via `list_skills` / `Skill`) and — unlike a static
//! in-process registry — survives a process restart.
//!
//! The official `@anthropic-ai/sdk` has no typed binding for these, so a TS client
//! drives them through the low-level `client.post` / `client.get` request methods.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::host::SharedHost;

/// Mount the skills API over the host's durable skill catalog.
pub fn skills_router(host: Arc<SharedHost>) -> Router {
    Router::new()
        .route("/v1/skills", post(create_skill).get(list_skills))
        .with_state(host)
}

/// `POST /v1/skills` — persist a delivered skill. Body: `{ "id": "...", "content":
/// "<SKILL.md>" }`. Returns the safe id it was stored under. 400 when `id`/`content`
/// are missing; 409 when this server runs without a durable skill store.
async fn create_skill(
    State(host): State<Arc<SharedHost>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let id = body.get("id").and_then(|v| v.as_str());
    let content = body.get("content").and_then(|v| v.as_str());
    let (Some(id), Some(content)) = (id, content) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "skill needs a string `id` and `content`" })),
        )
            .into_response();
    };
    match host.skill_store_put(id, content) {
        Some(stored_id) => (
            StatusCode::OK,
            Json(json!({ "id": stored_id, "type": "skill" })),
        )
            .into_response(),
        None => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "this server has no durable skill store" })),
        )
            .into_response(),
    }
}

/// `GET /v1/skills` — the ids currently in the durable catalog.
async fn list_skills(State(host): State<Arc<SharedHost>>) -> impl IntoResponse {
    let data: Vec<Value> = host
        .skill_store_list()
        .into_iter()
        .map(|id| json!({ "id": id, "type": "skill" }))
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "data": data, "has_more": false })),
    )
}
