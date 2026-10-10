// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This read-only Managed adapter delegates to the canonical Session application
// @formal reason: observation and performs only wire projection; it owns no durable fact or execution
// @formal reason: authority, and this link is not a formal-proof claim.
//! Awaken-only read projection for exact Session execution evidence.

use crate::common::scope::RequiredWorkspaceScope;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};

use super::{WireErr, session_projection_error};
use crate::state::ManagedState;

fn project_execution_evidence(
    observation: awaken_session_application::SessionExecutionObservation,
) -> crate::awaken_extensions::SessionExecutionEvidence {
    let public_thread_id =
        crate::state::public_thread_id(&observation.session_id, &observation.session_id);
    crate::awaken_extensions::SessionExecutionEvidence {
        session_id: observation.session_id,
        session_revision: observation.session_revision,
        agent_id: observation.agent_id,
        agent_revision: observation.agent_revision,
        model: observation.model,
        execution_model_ref: observation.execution_model_ref,
        backend_ref: observation.backend_ref,
        execution: observation.execution,
        runs: observation
            .runs
            .into_iter()
            .map(
                |run| crate::awaken_extensions::SessionRunExecutionEvidence {
                    run_id: run.run_id,
                    idempotency_key: run.idempotency_key,
                    state: run.state,
                    tool_use_ids: run
                        .tool_uses
                        .into_iter()
                        .map(|tool_use| {
                            crate::project::managed_tool_event_id(
                                &public_thread_id,
                                &tool_use.source_message_id,
                                &tool_use.call_id,
                            )
                        })
                        .collect(),
                    model_requests: run
                        .model_requests
                        .into_iter()
                        .map(
                            |request| crate::awaken_extensions::SessionModelRequestEvidence {
                                sequence: request.sequence,
                                is_error: request.is_error,
                                retry_count: request.retry_count,
                            },
                        )
                        .collect(),
                },
            )
            .collect(),
    }
}

/// Read immutable backend/model selection together with committed Run/model
/// observations. The Session application enforces ownership and reads both
/// existing authorities; this adapter performs only wire projection.
pub async fn get_session_execution_evidence(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(owner_scope): RequiredWorkspaceScope,
) -> Result<Json<crate::awaken_extensions::SessionExecutionEvidence>, WireErr> {
    let observation = state
        .roles
        .observation
        .session_execution_observation(&owner_scope, &awaken_session_types::SessionId::from(id))
        .await
        .map_err(session_projection_error)?;
    Ok(Json(project_execution_evidence(observation)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::{Id as RunId, RunState};

    #[test]
    fn projection_is_lossless_and_adds_no_execution_claim() {
        // Cause/effect decision table: P1 one application observation carries
        // frozen selection plus two Run facts; P2 one Run has no completed
        // model request. Effects: E1 every coordinate is preserved; E2 the
        // empty Run stays empty rather than being reported as executed. Rules:
        // R1=P1=>E1 and R2=P2=>E2.
        let projected =
            project_execution_evidence(awaken_session_application::SessionExecutionObservation {
                session_id: "session-1".into(),
                session_revision: 5,
                agent_id: "agent-1".into(),
                agent_revision: Some(3),
                model: "deepseek-v4-pro".into(),
                execution_model_ref: "deepseek-v4-pro".into(),
                backend_ref: Some("native".into()),
                execution: awaken_session_state_contract::SessionExecutionState::Running,
                runs: vec![
                    awaken_session_application::SessionRunExecutionObservation {
                        run_id: RunId("run-observed".into()),
                        idempotency_key: Some("command-observed".into()),
                        state: RunState::Running,
                        tool_uses: vec![awaken_session_application::SessionToolUseObservation {
                            source_message_id: "run-observed-assistant-0".into(),
                            call_id: "tool-submit".into(),
                        }],
                        model_requests: vec![
                            awaken_session_application::SessionModelRequestObservation {
                                sequence: 11,
                                is_error: false,
                                retry_count: 0,
                            },
                        ],
                    },
                    awaken_session_application::SessionRunExecutionObservation {
                        run_id: RunId("run-unobserved".into()),
                        idempotency_key: None,
                        state: RunState::Running,
                        tool_uses: Vec::new(),
                        model_requests: Vec::new(),
                    },
                ],
            });
        assert_eq!(projected.backend_ref.as_deref(), Some("native"), "R1/E1");
        assert_eq!(projected.runs[0].run_id.0, "run-observed", "R1/E1");
        assert_eq!(
            projected.runs[0].idempotency_key.as_deref(),
            Some("command-observed"),
            "R1/E1"
        );
        assert_eq!(
            projected.runs[0].tool_use_ids,
            [crate::project::managed_tool_event_id(
                &crate::state::public_thread_id("session-1", "session-1"),
                "run-observed-assistant-0",
                "tool-submit",
            )],
            "R1/E1"
        );
        assert_eq!(projected.runs[0].model_requests[0].sequence, 11, "R1/E1");
        assert!(projected.runs[1].model_requests.is_empty(), "R2/E2");
    }
}
