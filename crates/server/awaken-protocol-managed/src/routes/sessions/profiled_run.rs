//! Private profiled-Session Run lowering over canonical Session admission.

use crate::common::scope::RequiredWorkspaceScope;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};

use super::{WireErr, error_response, session_projection_error};
use crate::state::{ManagedState, StateError};
use awaken_session_types::RunError;

fn exact_receipt(
    session_id: &str,
    run_id: &awaken_agent_contract::agent::run::Id,
    admitted: &awaken_session_execution_contract::AdmittedSessionRun,
) -> Result<crate::awaken_extensions::ProfiledSessionRunReceipt, WireErr> {
    if admitted.session_id() != session_id || admitted.run_id() != run_id {
        return Err(error_response(StateError::Run(RunError::internal(
            "Session Run admission returned a mismatched identity",
        ))));
    }
    Ok(crate::awaken_extensions::ProfiledSessionRunReceipt {
        session_id: session_id.to_string(),
        run_id: run_id.clone(),
    })
}

/// Read the exact admitted native Run without consulting Managed input Events.
pub async fn get_profiled_session_run_events(
    State(state): State<Arc<ManagedState>>,
    Path((id, run_id)): Path<(String, String)>,
    RequiredWorkspaceScope(owner_scope): RequiredWorkspaceScope,
    Query(query): Query<crate::awaken_extensions::ProfiledSessionRunEventsQuery>,
) -> Result<Json<awaken_dispatch_contract::SessionRunEventPage>, WireErr> {
    state
        .roles
        .profiled
        .session_run_events_after(
            &owner_scope,
            &awaken_session_types::SessionId::from(id),
            &awaken_agent_contract::agent::run::Id(run_id),
            awaken_dispatch_contract::DispatchCursor(query.dispatch_after),
            awaken_agent_contract::RunLifecycleCursor(query.lifecycle_after),
            query.limit,
        )
        .await
        .map(Json)
        .map_err(session_projection_error)
}

/// Reuse Session reply admission, native ticket validation and durable receipt.
pub async fn resume_profiled_session_run(
    State(state): State<Arc<ManagedState>>,
    Path((id, run_id)): Path<(String, String)>,
    RequiredWorkspaceScope(owner_scope): RequiredWorkspaceScope,
    body: Result<Json<crate::awaken_extensions::ProfiledSessionRunResume>, JsonRejection>,
) -> Result<Json<crate::awaken_extensions::ProfiledSessionRunReceipt>, WireErr> {
    let Json(body) = body.map_err(|rejection| {
        error_response(StateError::Run(RunError::bad_request(
            crate::routes::extractors::managed_json_message(rejection.body_text()),
        )))
    })?;
    if body.call_id.as_ref().is_some_and(|id| id.trim().is_empty()) {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "native reply call id is empty",
        ))));
    }
    let receipt = crate::awaken_extensions::ProfiledSessionRunReceipt {
        session_id: id.clone(),
        run_id: awaken_agent_contract::agent::run::Id(run_id),
    };
    let command = body.into_command(
        awaken_session_types::SessionId::from(id),
        receipt.run_id.clone(),
    );
    state
        .roles
        .profiled
        .reply_profiled_session_run_for_owner(&owner_scope, command)
        .await
        .map_err(session_projection_error)?;
    Ok(Json(receipt))
}

/// Lower one private product-authored Run into the canonical Session Run
/// admission and activation state machine. The edge requires an existing
/// profiled aggregate first, so it cannot exercise the ordinary public
/// protocol's implicit Session-creation compatibility path.
pub async fn submit_profiled_session_run(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(owner_scope): RequiredWorkspaceScope,
    body: Result<Json<crate::awaken_extensions::ProfiledSessionRunSubmit>, JsonRejection>,
) -> Result<Json<crate::awaken_extensions::ProfiledSessionRunReceipt>, WireErr> {
    let Json(body) = body.map_err(|rejection| {
        error_response(StateError::Run(RunError::bad_request(
            crate::routes::extractors::managed_json_message(rejection.body_text()),
        )))
    })?;
    let projection = state
        .roles
        .projection
        .read_session_projection(&id, Some(&owner_scope))
        .await
        .map_err(session_projection_error)?
        .ok_or_else(|| error_response(StateError::NotFound))?;
    let baseline = projection
        .session
        .frozen_baseline()
        .ok_or_else(|| error_response(StateError::NotFound))?;
    if baseline.mutation_policy.is_managed() {
        return Err(error_response(StateError::NotFound));
    }
    if baseline.agent_id != body.agent_id {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "profiled Session Run Agent does not match the frozen Session",
        ))));
    }

    let run_id = body.run_id.clone();
    let admitted = Box::pin(state.roles.profiled.admit_session_run_for_owner(
        &owner_scope,
        awaken_session_execution_contract::AdmitSessionRun {
            session_id: awaken_session_types::SessionId::from(&id),
            agent_id: body.agent_id,
            operation_id: body.operation_id,
            run_id: body.run_id,
            messages: body.messages,
            data_subject_id: None,
            traceparent: None,
            execution_requirements: body.execution_requirements,
            replacement: awaken_session_types::SessionRunReplacement::PreservePrior,
        },
    ))
    .await
    .map_err(|error| error_response(StateError::Run(error)))?;
    let receipt = exact_receipt(&id, &run_id, &admitted)?;
    Box::pin(state.roles.profiled.activate_admitted_session_run(admitted))
        .await
        .map_err(|error| error_response(StateError::Run(error)))?;
    Ok(Json(receipt))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::exact_receipt;
    use crate::routes::sessions::session_projection_error;
    use awaken_session_types::RunError;

    #[test]
    fn receipt_is_checked_before_activation_linkage() {
        // Causes: C1 admitted Session/Run equal the requested path/body; C2 one
        // coordinate differs. Effects: E1 C1 returns the exact minimal receipt;
        // E2 C2 is a typed 500 before the caller can pass admission to
        // activation. Rules R1=C1=>E1 and R8=C2=>E2.
        let run_id = awaken_agent_contract::agent::run::Id("run-1".into());
        let exact = awaken_session_execution_contract::AdmittedSessionRun::Completed {
            session_id: "session-1".into(),
            run_id: run_id.clone(),
        };
        assert_eq!(
            exact_receipt("session-1", &run_id, &exact).unwrap(),
            crate::awaken_extensions::ProfiledSessionRunReceipt {
                session_id: "session-1".into(),
                run_id: run_id.clone(),
            },
            "R1/E1"
        );
        let changed = awaken_session_execution_contract::AdmittedSessionRun::Completed {
            session_id: "other-session".into(),
            run_id: run_id.clone(),
        };
        let (status, _) = exact_receipt("session-1", &run_id, &changed).unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "R8/E2 Session");
        let changed = awaken_session_execution_contract::AdmittedSessionRun::Completed {
            session_id: "session-1".into(),
            run_id: awaken_agent_contract::agent::run::Id("other-run".into()),
        };
        let (status, _) = exact_receipt("session-1", &run_id, &changed).unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "R8/E2 Run");
    }

    #[test]
    fn projection_errors_preserve_the_existing_wire_classes() {
        // Causes: C1 no readable Session; C2 deterministic admission rejection;
        // C3 an internal admission invariant; C4 admission dependency outage;
        // C5 projection storage outage. Effects: E1 404/not_found_error; E2
        // 400/invalid_request_error; E3 500/api_error; E4/E5 503/api_error.
        // Decision rules M1=C1=>E1, M2=C2=>E2, M3=C3=>E3,
        // M4=C4=>E4, and M5=C5=>E5. The adapter reuses the standard Managed
        // error envelope; it owns no retry classifier or message parser.
        use awaken_session_application::SessionProjectionRecoveryError as Error;

        for (rule, error, expected_status, expected_kind) in [
            (
                "M1",
                Error::NotFound,
                StatusCode::NOT_FOUND,
                "not_found_error",
            ),
            (
                "M2",
                Error::Rejected(RunError::bad_request("rejected")),
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
            ),
            (
                "M3",
                Error::Rejected(RunError::internal("invariant")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
            ),
            (
                "M4",
                Error::Rejected(RunError::unavailable("dependency")),
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
            ),
            (
                "M5",
                Error::Unavailable("repository".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
            ),
        ] {
            let (status, body) = session_projection_error(error);
            assert_eq!(status, expected_status, "{rule}");
            assert_eq!(body.0.error.kind, expected_kind, "{rule}");
        }
    }
}
