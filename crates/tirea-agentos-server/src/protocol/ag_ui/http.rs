use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::json;
use tirea_agentos::orchestrator::AgentOsRunError;
use tirea_protocol_ag_ui::{AgUiHistoryEncoder, AgUiProtocolEncoder, Event, RunAgentInput};

use super::runtime::apply_agui_extensions;

use crate::service::{
    encode_message_page, forward_dialog_decisions_by_thread, load_message_page,
    prepare_http_dialog_run, ApiError, AppState, MessageQueryParams,
};
use crate::transport::http_run::{wire_http_sse_relay, HttpSseRelayConfig};
use crate::transport::http_sse::{sse_body_stream, sse_response};

const RUN_PATH: &str = "/agents/:agent_id/runs";
const THREAD_MESSAGES_PATH: &str = "/threads/:id/messages";

/// Build AG-UI HTTP routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(RUN_PATH, post(run))
        .route(THREAD_MESSAGES_PATH, get(thread_messages))
}

async fn thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MessageQueryParams>,
) -> Result<impl IntoResponse, ApiError> {
    let page = load_message_page(&st.read_store, &id, &params).await?;
    let encoded = encode_message_page(page, AgUiHistoryEncoder::encode_message);
    Ok(Json(encoded))
}

async fn run(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<RunAgentInput>,
) -> Result<Response, ApiError> {
    req.validate()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let frontend_run_id = req.run_id.clone();

    let suspension_decisions = req.suspension_decisions();
    let maybe_forwarded = forward_dialog_decisions_by_thread(
        &st.os,
        &agent_id,
        &req.thread_id,
        req.has_user_input(),
        Some(frontend_run_id.as_str()),
        &suspension_decisions,
    )
    .await?;
    if let Some(forwarded) = maybe_forwarded {
        return Ok((
            axum::http::StatusCode::ACCEPTED,
            Json(json!({
                "status": "decision_forwarded",
                "threadId": forwarded.thread_id,
                "runId": frontend_run_id,
            })),
        )
            .into_response());
    }

    let mut resolved = st.os.resolve(&agent_id).map_err(AgentOsRunError::from)?;
    apply_agui_extensions(&mut resolved, &req)
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    let run_request = req.into_runtime_run_request(agent_id.clone());

    let prepared = prepare_http_dialog_run(&st.os, resolved, run_request, &agent_id).await?;
    let run_id_for_cleanup = prepared.run_id.clone();
    let os_for_cleanup = st.os.clone();

    let enc = AgUiProtocolEncoder::new_with_frontend_run_id(frontend_run_id);
    let sse_rx = wire_http_sse_relay(
        prepared.starter,
        enc,
        prepared.ingress_rx,
        HttpSseRelayConfig {
            thread_id: prepared.thread_id,
            fanout: None,
            resumable_downstream: false,
            protocol_label: "ag-ui",
            on_relay_done: move |_sse_tx| async move {
                os_for_cleanup
                    .remove_thread_run_handle(&run_id_for_cleanup)
                    .await;
            },
            error_formatter: |msg| {
                let json =
                    serde_json::to_string(&Event::run_error(&msg, Some("RELAY_ERROR".to_string())))
                        .unwrap_or_default();
                Bytes::from(format!("data: {json}\n\n"))
            },
        },
    );

    Ok(sse_response(sse_body_stream(sse_rx)))
}
