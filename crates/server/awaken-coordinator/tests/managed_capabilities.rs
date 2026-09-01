//! A managed session advertises the host's *real* provisioned surface on its agent
//! object in the official Managed Agents wire shapes: the built-in tools fold into a
//! single `agent_toolset_20260401` reference (unregistered tools disabled,
//! confirmation-gated tools `always_ask`), client tools become `custom` definitions,
//! only versioned catalog skills become `custom` skill references, and a delegate
//! roster becomes a `coordinator` multiagent object. This drives the full
//! `ManagedHost::capabilities` → host accessors → `project` wiring over the public
//! wire.

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
            serde_json::to_vec(&serde_json::json!({
                "agent": "assistant",
                "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// The built-in hand tools fold into one `agent_toolset_20260401` reference. This
/// legacy fixture has no typed Agent Toolset, so the closed fallback keeps
/// read/glob/grep on the Managed Agent `always_allow` default and projects
/// bash/write/edit as enabled `always_ask` overrides. Configurable `web_fetch`
/// and `web_search` remain disabled when no routed Web plugin is selected. An
/// unversioned host-static Skill remains direct-session compatibility input and
/// does not appear on Managed `agent.skills`.
///
/// Cause/effect graph and decision table:
/// C1=no typed Agent Toolset is bound, C2=the registered closed member is a
/// controlled modification, C3=the registered closed member is perception,
/// C4=a configurable Web plugin is absent, C5=an unversioned host-static Skill
/// exists; E1=perception inherits the enabled/always-allow default,
/// E2=controlled modification is explicitly enabled/always-ask, E3=the Web tool
/// is disabled with the default policy repeated in its complete config,
/// E4=the direct-only Skill is absent from Managed capabilities.
/// R1(C1,C3)->E1 covers read/glob/grep and therefore emits no override;
/// R2(C1,C2)->E2 covers bash/write/edit;
/// R3(C4)->E3 covers WebFetch and WebSearch;
/// R4(C5, no versioned catalog record)->E4 covers profile convergence.
/// Constraints/invariants: one neutral `SessionToolConfiguration` owns policy;
/// the Managed projector is the only wire owner, Web providers use the unified
/// configured-provider registry, and there is no legacy static WebFetch path.
/// FMECA: advertising an absent Web route would create an inexecutable tool (fail
/// closed through E3); losing an enabled tool's approval policy could perform an
/// effect without a configured executor (caught by E2). Exact permission
/// overrides are covered by the neutral-policy and runtime-gate decision tables.
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
            // Every config carries all of {name, type, enabled, permission_policy}
            // as the `BetaManagedAgentsAgentToolConfig` SDK type requires; only
            // deviations from `default_config` are listed. Legacy perception
            // members inherit always_allow, controlled modifications require
            // confirmation, and unavailable Web members flip only `enabled`.
            "configs": [
                { "name": "bash", "type": "bash", "enabled": true, "permission_policy": { "type": "always_ask" } },
                { "name": "write", "type": "write", "enabled": true, "permission_policy": { "type": "always_ask" } },
                { "name": "edit", "type": "edit", "enabled": true, "permission_policy": { "type": "always_ask" } },
                { "name": "web_fetch", "type": "web_fetch", "enabled": false, "permission_policy": { "type": "always_allow" } },
                { "name": "web_search", "type": "web_search", "enabled": false, "permission_policy": { "type": "always_allow" } }
            ],
            "default_config": { "enabled": true, "permission_policy": { "type": "always_allow" } }
        }])
    );
    assert_eq!(session["agent"]["skills"], serde_json::json!([]));
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
