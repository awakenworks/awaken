//! A managed session advertises the host's *real* provisioned surface on its agent
//! object in the official Managed Agents wire shapes: the built-in tools fold into a
//! single `agent_toolset_20260401` reference (unregistered tools disabled,
//! confirmation-gated tools `always_ask`), client tools become `custom` definitions,
//! offered skills become `custom` skill references, and a delegate roster becomes a
//! `coordinator` multiagent object. This drives the full `ManagedHost::capabilities`
//! → host accessors → `project` wiring over the public wire.

use std::sync::Arc;

use awaken_coordinator::SkillSpec;
use awaken_scenario_host::{
    EchoModel, build_custom_router, build_delegation_router, build_router_with_skills,
};
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

/// The built-in hand tools fold into one `agent_toolset_20260401` reference: the
/// registered tools stay (read/glob/grep auto-allowed → toolset default;
/// bash/write/edit/web_fetch gated → `always_ask`), while configurable
/// `web_search` is disabled when no search connector is published. Offered skills
/// appear on `agent.skills`.
///
/// Cause/effect graph and decision table:
/// C1=the host registers a static tool, C2=the effective policy requires approval,
/// C3=a configurable connector is absent; E1=the tool remains enabled,
/// E2=its wire policy is `always_ask`, E3=the tool is disabled.
/// R1(C1,C2,!C3)->E1+E2 covers `web_fetch`; R2(!C1,!C2,C3)->E3 covers
/// `web_search`; R3(C1,!C2,!C3)->the default config covers read/glob/grep.
/// FMECA: advertising absent search would create an inexecutable tool (fail closed
/// through E3); disabling fetch would hide a real capability (caught by R1); losing
/// the approval policy could perform network I/O without consent (caught by E2).
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
            // Every config carries all of {name, enabled, permission_policy} as the
            // `BetaManagedAgentsAgentToolConfig` SDK type requires; only deviations
            // from `default_config` (enabled + auto-allowed) are listed — gated
            // tools flip the policy, unregistered tools flip `enabled`.
            "configs": [
                { "name": "bash", "enabled": true, "permission_policy": { "type": "always_ask" } },
                { "name": "write", "enabled": true, "permission_policy": { "type": "always_ask" } },
                { "name": "edit", "enabled": true, "permission_policy": { "type": "always_ask" } },
                { "name": "web_fetch", "enabled": true, "permission_policy": { "type": "always_ask" } },
                { "name": "web_search", "enabled": false, "permission_policy": { "type": "always_allow" } }
            ],
            "default_config": { "enabled": true, "permission_policy": { "type": "always_allow" } }
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

/// A delegate roster is advertised as a `coordinator` multiagent object containing
/// the complete frozen child Agent definitions, not a parallel list of ids.
///
/// Cause/effect graph and decision table:
/// C1=root publication delegates to researcher, C2=researcher publication resolves,
/// C3=the resolved publication has an exact revision; E1=coordinator is advertised,
/// E2=one full child object carries id/name/type/version.
/// R1(C1,C2,C3)->E1+E2. A missing C2 or C3 fails Session creation in production and
/// is covered at the projection boundary. FMECA: id-only projection loses frozen
/// child behavior after catalog drift; asserting every identity/revision field
/// detects that high-severity compatibility regression.
#[tokio::test]
async fn managed_session_advertises_multiagent_roster() {
    let app = build_delegation_router();
    let session = create_session(&app).await;

    assert_eq!(session["agent"]["multiagent"]["type"], "coordinator");
    let agents = session["agent"]["multiagent"]["agents"].as_array().unwrap();
    let researcher = agents
        .iter()
        .find(|agent| agent["id"] == "researcher")
        .expect("the frozen researcher Agent is advertised");
    assert_eq!(researcher["name"], "researcher");
    assert_eq!(researcher["type"], "agent");
    assert_eq!(researcher["version"], 1);
}
