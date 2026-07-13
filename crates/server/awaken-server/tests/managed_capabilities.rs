//! A managed session advertises the host's *real* provisioned surface on its agent
//! object in the official Managed Agents wire shapes: the built-in tools fold into a
//! single `agent_toolset_20260401` reference (unregistered tools disabled,
//! confirmation-gated tools `always_ask`), client tools become `custom` definitions,
//! offered skills become `custom` skill references, and a delegate roster becomes a
//! `coordinator` multiagent object. This drives the full `ManagedHost::capabilities`
//! → host accessors → `project` wiring over the public wire.

use std::sync::Arc;

use awaken_server::{SkillSpec};
use awaken_scenario_host::{EchoModel, build_custom_router, build_delegation_router, build_router_with_skills};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn create_session(app: &Router) -> serde_json::Value {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "agent": "assistant" })).unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// The built-in hand tools fold into one `agent_toolset_20260401` reference: the six
/// registered tools stay (read/glob/grep auto-allowed → toolset default;
/// bash/write/edit gated → `always_ask`), and the two the host doesn't register
/// (web_fetch/web_search) are disabled. Offered skills appear on `agent.skills`.
#[tokio::test]
async fn managed_session_folds_builtins_into_the_agent_toolset() {
    let skill = SkillSpec::new(
        "deploy",
        "Deploy",
        "Deploy checklist",
        "STEP: migrate first.",
    );
    let app = build_router_with_skills(Arc::new(EchoModel), "echo-model", vec![skill]);

    let session = create_session(&app).await;

    assert_eq!(
        session["agent"]["tools"],
        serde_json::json!([{
            "type": "agent_toolset_20260401",
            "configs": [
                { "name": "bash", "permission_policy": { "type": "always_ask" } },
                { "name": "write", "permission_policy": { "type": "always_ask" } },
                { "name": "edit", "permission_policy": { "type": "always_ask" } },
                { "name": "web_fetch", "enabled": false },
                { "name": "web_search", "enabled": false }
            ]
        }])
    );
    assert_eq!(
        session["agent"]["skills"],
        serde_json::json!([{ "type": "custom", "skill_id": "deploy", "version": "latest" }])
    );
    assert!(session["agent"]["multiagent"].is_null());
    assert!(session["resources"].as_array().unwrap().is_empty());
}

/// A client-executed tool is advertised as a `custom` tool definition alongside the
/// built-in toolset reference.
#[tokio::test]
async fn managed_session_advertises_custom_tools() {
    let app = build_custom_router();
    let session = create_session(&app).await;

    let tools = session["agent"]["tools"].as_array().unwrap();
    assert_eq!(tools[0]["type"], "agent_toolset_20260401");
    let custom = tools
        .iter()
        .find(|t| t["type"] == "custom")
        .expect("a custom tool is advertised");
    assert_eq!(custom["name"], "submit_answer");
    assert!(custom["input_schema"].is_object());
}

/// A delegate roster is advertised as a `coordinator` multiagent object naming the
/// delegate agent ids.
#[tokio::test]
async fn managed_session_advertises_multiagent_roster() {
    let app = build_delegation_router();
    let session = create_session(&app).await;

    assert_eq!(session["agent"]["multiagent"]["type"], "coordinator");
    let agents = session["agent"]["multiagent"]["agents"].as_array().unwrap();
    assert!(agents.iter().any(|a| a == "researcher"));
}
