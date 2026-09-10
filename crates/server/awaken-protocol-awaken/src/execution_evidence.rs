//! Read-only Awaken extension for exact Session execution evidence.

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_session_contract::SessionExecutionState;
use axum::Router;
use axum::handler::Handler;
use axum::routing::get;
use serde::{Deserialize, Serialize};

/// Product-facing projection of one immutable Session execution selection and
/// its committed primary-Thread Runs. This is evidence, not another authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExecutionEvidence {
    pub session_id: String,
    pub session_revision: u64,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_revision: Option<u64>,
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
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub state: RunState,
    pub tool_use_ids: Vec<String>,
    pub model_requests: Vec<SessionModelRequestEvidence>,
}

/// Minimal durable proof of one completed logical model request. Token usage is
/// intentionally absent because the Managed Events API already owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionModelRequestEvidence {
    pub sequence: u64,
    pub is_error: bool,
    pub retry_count: u32,
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

    #[test]
    fn evidence_wire_is_exact_and_rejects_unknown_fields() {
        // Cause/effect decision table: W1 complete evidence round-trips every
        // frozen and committed coordinate; W2 omitted optional coordinates stay
        // absent; W3 an unknown field is rejected. Effects: E1 no evidence
        // identity is lost; E2 historical unknown is not invented; E3 schema
        // drift fails closed. Rules R1=W1=>E1, R2=W2=>E2, R3=W3=>E3.
        let evidence = SessionExecutionEvidence {
            session_id: "session-1".into(),
            session_revision: 4,
            agent_id: "agent-1".into(),
            agent_revision: Some(7),
            model: "deepseek-v4-pro".into(),
            execution_model_ref: "deepseek-v4-pro".into(),
            backend_ref: Some("acp:claude".into()),
            execution: SessionExecutionState::Idle,
            runs: vec![SessionRunExecutionEvidence {
                run_id: RunId("run-1".into()),
                idempotency_key: Some("command-1".into()),
                state: RunState::Running,
                tool_use_ids: vec!["tool-submit".into()],
                model_requests: vec![SessionModelRequestEvidence {
                    sequence: 9,
                    is_error: false,
                    retry_count: 1,
                }],
            }],
        };
        let wire = serde_json::to_value(&evidence).expect("R1 serialize");
        assert_eq!(
            serde_json::from_value::<SessionExecutionEvidence>(wire.clone())
                .expect("R1 deserialize"),
            evidence,
            "R1/E1"
        );

        let mut historical = wire.clone();
        historical.as_object_mut().unwrap().remove("agent_revision");
        historical.as_object_mut().unwrap().remove("backend_ref");
        historical["runs"][0]
            .as_object_mut()
            .unwrap()
            .remove("idempotency_key");
        let historical = serde_json::from_value::<SessionExecutionEvidence>(historical)
            .expect("R2 optional coordinates");
        assert_eq!(historical.agent_revision, None, "R2/E2");
        assert_eq!(historical.backend_ref, None, "R2/E2");
        assert_eq!(historical.runs[0].idempotency_key, None, "R2/E2");

        let mut drifted = wire;
        drifted["configured_backend"] = serde_json::json!("acp:claude");
        assert!(
            serde_json::from_value::<SessionExecutionEvidence>(drifted).is_err(),
            "R3/E3"
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
            Json(SessionExecutionEvidence {
                session_id,
                session_revision: 1,
                agent_id: "agent".into(),
                agent_revision: None,
                model: "model".into(),
                execution_model_ref: "model".into(),
                backend_ref: None,
                execution: SessionExecutionState::Idle,
                runs: Vec::new(),
            })
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
