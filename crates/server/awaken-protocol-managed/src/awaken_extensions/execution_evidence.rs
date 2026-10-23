// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This read-only wire DTO projects the canonical Session application and committed Run
// @formal reason: facts; it owns no mutation or execution authority, and this product-requirement link is
// @formal reason: not a formal-proof claim.
//! Read-only Awaken extension for exact Session execution evidence.

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_session_state_contract::SessionExecutionState;
use axum::{Router, handler::Handler, routing::get};
use serde::{Deserialize, Serialize};

/// Product-facing projection of one immutable Session execution selection and
/// its committed primary-Thread Runs. This is evidence, not another authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExecutionEvidence {
    /// SDK: none — optional safe original-configuration inspection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration:
        Option<awaken_session_configuration_contract::SessionConfigurationInspection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation: Option<awaken_session_execution_contract::SessionRecoveryPreparationReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_capture: Option<awaken_resource_contract::ArtifactCapture>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_footprint:
        Option<awaken_session_execution_contract::SessionExecutionOperationFootprint>,
    /// Expanded original-operation read, explicitly selected by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_inventory:
        Option<awaken_session_execution_contract::SessionExecutionOperationInventory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<awaken_session_execution_contract::SessionExecutionRecoveryObservation>,
    pub session_id: String,
    pub session_revision: u64,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_revision: Option<u64>,
    /// Original non-bearer provenance, not mutable metadata or a current grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_origin: Option<awaken_session_configuration_contract::ApplicationOriginBinding>,
    pub model: String,
    pub execution_model_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_ref: Option<String>,
    pub execution: SessionExecutionState,
    pub runs: Vec<SessionRunExecutionEvidence>,
}

/// One committed Run and its model-request completion coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRunExecutionEvidence {
    /// SDK: none — recorded attempts from the canonical model journal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_operations: Vec<awaken_runtime_contract::model_inspection::ModelOperationInspection>,
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub state: RunState,
    pub tool_use_ids: Vec<String>,
    pub model_requests: Vec<awaken_session_execution_contract::SessionModelRequestObservation>,
}

/// Mount the read-only evidence adapter under Awaken's extension namespace.
pub fn session_execution_evidence_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/sessions/{id}/execution-evidence", get(handler))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence_fixture() -> SessionExecutionEvidence {
        SessionExecutionEvidence {
            configuration: None,
            preparation: None,
            artifact_capture: None,
            operation_footprint: None,
            operation_inventory: None,
            recovery: None,
            session_id: "session-1".into(),
            session_revision: 4,
            agent_id: "agent-1".into(),
            agent_revision: Some(7),
            application_origin: Some(
                awaken_session_configuration_contract::ApplicationOriginBinding::new(
                    "application.interaction:v1".into(),
                    "a".repeat(64),
                    "opaque-origin-proof".into(),
                )
                .unwrap(),
            ),
            model: "deepseek-v4-pro".into(),
            execution_model_ref: "deepseek-v4-pro".into(),
            backend_ref: Some("acp:claude".into()),
            execution: SessionExecutionState::Idle,
            runs: vec![SessionRunExecutionEvidence {
                model_operations: Vec::new(),
                run_id: RunId("run-1".into()),
                idempotency_key: Some("command-1".into()),
                state: RunState::Running,
                tool_use_ids: vec!["tool-submit".into()],
                model_requests: vec![
                    awaken_session_execution_contract::SessionModelRequestObservation {
                        sequence: 9,
                        is_error: false,
                        retry_count: 1,
                    },
                ],
            }],
        }
    }

    #[test]
    fn evidence_wire_is_exact_and_rejects_unknown_fields() {
        // Cause/effect decision table: W1 complete evidence round-trips every
        // frozen and committed coordinate; W2 omitted optional coordinates stay
        // absent; W3 an unknown field is rejected. Effects: E1 no evidence
        // identity is lost; E2 historical unknown is not invented; E3 schema
        // drift fails closed. Rules R1=W1=>E1, R2=W2=>E2, R3=W3=>E3.
        let evidence = evidence_fixture();
        let wire = serde_json::to_value(&evidence).expect("R1 serialize");
        assert_eq!(
            serde_json::from_value::<SessionExecutionEvidence>(wire.clone())
                .expect("R1 deserialize"),
            evidence,
            "R1/E1"
        );

        let mut historical = wire.clone();
        historical.as_object_mut().unwrap().remove("agent_revision");
        historical
            .as_object_mut()
            .unwrap()
            .remove("application_origin");
        historical.as_object_mut().unwrap().remove("backend_ref");
        historical["runs"][0]
            .as_object_mut()
            .unwrap()
            .remove("idempotency_key");
        let historical = serde_json::from_value::<SessionExecutionEvidence>(historical)
            .expect("R2 optional coordinates");
        assert_eq!(historical.agent_revision, None, "R2/E2");
        assert_eq!(
            historical.application_origin, None,
            "R2 legacy has no invented origin"
        );
        assert_eq!(historical.backend_ref, None, "R2/E2");
        assert_eq!(historical.runs[0].idempotency_key, None, "R2/E2");

        let mut drifted = wire;
        drifted["configured_backend"] = serde_json::json!("acp:claude");
        assert!(
            serde_json::from_value::<SessionExecutionEvidence>(drifted).is_err(),
            "R3/E3"
        );
        // R4 a new optional footprint is an outer projection. Existing strict
        // recovery readers retain exactly their four closure coordinates; old
        // evidence omits the new outer field and never invents this observation.
        let mut with_footprint = evidence;
        // W4 optional original capture is lossless; legacy None above retains
        // its exact old wire shape. The reference is not a cleanup/Run grant.
        with_footprint.artifact_capture=Some(serde_json::from_value(serde_json::json!({
            "workspace_id":"workspace","session_id":"session-1","revision":2,
            "file_ids":["file-original"],"origin":{"run_id":"original-run","owner":"worker","epoch":7}
        })).unwrap());
        with_footprint.operation_footprint =
            Some(awaken_session_execution_contract::SessionExecutionOperationFootprint::ResponseOnly);
        with_footprint.preparation = Some(
            awaken_session_execution_contract::SessionRecoveryPreparationReference {
                session_id: "session-1".into(),
                effect_id: "original-effect".into(),
                preparation_fingerprint: "original-preparation".into(),
            },
        );
        with_footprint.recovery = Some(
            awaken_session_execution_contract::SessionExecutionRecoveryObservation::EffectsClosed {
                preparation: None,
                operation_footprint: None,
                operation_inventory: None,
                session_revision: 4,
                runtime_commit_cursor: 9,
                thread_ids: vec!["session-1".into()],
                closure_records: vec![
                    awaken_session_execution_contract::SessionExecutionClosureReference {
                        run_id: "run-1".into(),
                        evidence_ref: "original-closure".into(),
                    },
                ],
            },
        );
        let wire = serde_json::to_value(&with_footprint).unwrap();
        assert_eq!(
            serde_json::from_value::<SessionExecutionEvidence>(wire.clone()).unwrap(),
            with_footprint,
            "R4 exact outer preparation round-trip"
        );
        assert_eq!(
            wire["preparation"]["preparation_fingerprint"], "original-preparation",
            "R4 original reference"
        );
        assert_eq!(
            wire["artifact_capture"]["origin"]["run_id"], "original-run",
            "W4 original source"
        );
        assert_eq!(wire["operation_footprint"], "response_only", "R4");
        assert!(
            wire["recovery"].get("operation_footprint").is_none(),
            "R4 legacy recovery shape retained"
        );
    }

    #[tokio::test]
    async fn evidence_route_inventory_has_one_get_only_owner() {
        // Cause/effect decision table: I1 exact path+GET reaches the injected
        // reader; I2 the same path+POST is 405; I3 a lookalike path is 404.
        // The extension crate owns one unprefixed route and no compatibility
        // alias. Rules R4=I1=>200, R5=I2=>405, R6=I3=>404.
        use axum::Json;
        use axum::body::Body;
        use axum::extract::Path;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        async fn read(Path(session_id): Path<String>) -> Json<SessionExecutionEvidence> {
            let mut evidence = evidence_fixture();
            evidence.session_id = session_id;
            Json(evidence)
        }

        let app = session_execution_evidence_router(read);
        for (rule, method, uri, expected) in [
            (
                "R4",
                "GET",
                "/v1/awaken/sessions/session-1/execution-evidence",
                StatusCode::OK,
            ),
            (
                "R5",
                "POST",
                "/v1/awaken/sessions/session-1/execution-evidence",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                "R6",
                "GET",
                "/v1/awaken/sessions/session-1/execution",
                StatusCode::NOT_FOUND,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), expected, "{rule}");
        }
    }
}
