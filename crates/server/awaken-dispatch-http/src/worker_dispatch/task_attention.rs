// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: HTTP anti-corruption adapter: authenticates one registered Worker incarnation,
// @formal reason: binds the Task holder to it and forwards the attention to the Session
// @formal reason: application's existing admission. It owns no Task or Session state.
//! Durable delivery of a registered Worker's Task observations.
//!
//! A BackgroundTask runs in the Worker that holds its tenure, after the Run
//! that started it has settled, so no Run claim authorizes its result. The
//! authority is the Task holder: a registered Worker's holder names exactly
//! its own (Worker, incarnation), so an authenticated Worker can deliver only
//! the observations of Tasks it holds. The Session application then admits the
//! background Run that carries the observation; the response is the durable
//! acknowledgement.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::{Json, Router, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_dispatch::{WorkerDirectory, WorkerIdentity};
use awaken_session_application_contract::{
    SessionRunBackgroundApplication, TaskAttention, WORKER_TASK_ATTENTION_PATH,
    registered_task_holder,
};
use awaken_time::{WallClock, WallClockExt};
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, authenticate_worker_request,
    verify_current_worker_identity,
};

use super::{ApplicationError, respond};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskAttentionReq {
    identity: WorkerIdentity,
    attention: TaskAttention,
}

/// The Task-holder check (decision table rows A2/A3):
/// | Rule | holder | effect |
/// |---|---|---|
/// | A2 | exactly this Worker incarnation's registered holder | admitted |
/// | A3 | another Worker's or incarnation's holder, or a process holder | refused |
fn admit_task_holder(identity: &WorkerIdentity, holder: &str) -> Result<(), ApplicationError> {
    if holder == registered_task_holder(&identity.worker_id, &identity.incarnation_id) {
        Ok(())
    } else {
        Err(ApplicationError::invalid(
            "the Task attention names a Task this Worker incarnation does not hold",
        ))
    }
}

/// Map the application's refusal once: a definitive refusal stays definitive
/// (the Worker stops retrying, row K4); anything else is retryable (K5).
fn attention_failure(error: awaken_session_types::RunError) -> ApplicationError {
    match error.kind {
        awaken_session_types::RunErrorKind::BadRequest => {
            ApplicationError::invalid(error.to_string())
        }
        awaken_session_types::RunErrorKind::Conflict
        | awaken_session_types::RunErrorKind::Internal
        | awaken_session_types::RunErrorKind::Unavailable => {
            ApplicationError::unavailable(error.to_string())
        }
    }
}

/// Authenticated Task attention route. `attention` is the Coordinator's
/// Session Run application; `clock` judges the Worker's registered incarnation.
pub fn worker_task_attention_router(
    attention: Arc<dyn SessionRunBackgroundApplication>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WallClock>,
) -> Router {
    #[derive(Clone)]
    struct AttentionState {
        attention: Arc<dyn SessionRunBackgroundApplication>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        clock: Arc<dyn WallClock>,
    }

    async fn deliver(
        State(state): State<AttentionState>,
        Extension(worker): Extension<VerifiedWorkerContext>,
        Json(request): Json<TaskAttentionReq>,
    ) -> (StatusCode, Json<Value>) {
        let result = async {
            verify_current_worker_identity(
                state.directory.as_ref(),
                &worker,
                &request.identity,
                state.clock.now_ms(),
                false,
            )
            .await
            .map_err(ApplicationError::invalid)?;
            admit_task_holder(&request.identity, &request.attention.holder)?;
            let ack = state
                .attention
                .submit_task_attention(request.attention)
                .await
                .map_err(attention_failure)?;
            Ok(json!({ "ack": ack }))
        }
        .await;
        respond(result)
    }

    let state = AttentionState {
        attention,
        directory,
        authenticator,
        clock,
    };
    Router::new()
        .route(WORKER_TASK_ATTENTION_PATH, post(deliver))
        .route_layer(axum::middleware::from_fn_with_state(
            state.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(worker: &str, incarnation: &str) -> WorkerIdentity {
        WorkerIdentity {
            worker_id: worker.into(),
            incarnation_id: incarnation.into(),
            generation: 1,
        }
    }

    #[test]
    fn only_the_holding_worker_incarnation_may_deliver_task_attention() {
        // A2: the exact registered holder of the authenticated incarnation.
        let current = identity("worker-a", "incarnation-1");
        assert!(
            admit_task_holder(
                &current,
                &registered_task_holder("worker-a", "incarnation-1")
            )
            .is_ok()
        );
        // A3: another Worker, a previous incarnation, a co-located process
        // holder, or an empty holder.
        for holder in [
            registered_task_holder("worker-b", "incarnation-1"),
            registered_task_holder("worker-a", "incarnation-0"),
            "background-worker-process-fingerprint".to_owned(),
            String::new(),
        ] {
            assert!(admit_task_holder(&current, &holder).is_err(), "{holder}");
        }
    }

    #[test]
    fn a_definitive_application_refusal_stays_definitive() {
        // K4 BadRequest stays a refusal; K5 every other kind is retryable.
        use awaken_session_types::RunError;
        let invalid = attention_failure(RunError::bad_request("gone"));
        assert_eq!(
            invalid.kind,
            awaken_session_execution_contract::ApplicationErrorKind::InvalidRequest
        );
        for error in [RunError::unavailable("busy"), RunError::internal("store")] {
            assert_eq!(
                attention_failure(error).kind,
                awaken_session_execution_contract::ApplicationErrorKind::Unavailable
            );
        }
    }
}
