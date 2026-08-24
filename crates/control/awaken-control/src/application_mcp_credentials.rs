//! Awaken-owned hosted application credential admission.

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::ManagedCredentialAdoptionProgress;
use awaken_protocol_managed::{VaultState, parse_idempotency_key_header};
use awaken_tenancy::WorkspaceScope;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplicationMcpCredentialCommand {
    application_authority_id: String,
    mcp_server_url: String,
    /// Caller-owned monotonic order for this stable application/target tuple.
    credential_generation: u64,
    /// Write-only. The response and durable credential row never contain it.
    token: String,
}

#[derive(Serialize)]
struct ApplicationMcpCredentialReceipt {
    #[serde(rename = "type")]
    object_type: &'static str,
    vault_id: String,
    credential_source_id: String,
    revision: u64,
    adoption: ManagedCredentialAdoptionProgress,
}

type CommandError = (
    StatusCode,
    Json<awaken_protocol_managed::types::ErrorResponse>,
);

fn error(status: StatusCode, error_type: &'static str, message: impl Into<String>) -> CommandError {
    (
        status,
        Json(awaken_protocol_managed::types::ErrorResponse::new(
            error_type, message,
        )),
    )
}

async fn enter_application_mcp_credential(
    State(state): State<Arc<VaultState>>,
    Extension(workspace): Extension<WorkspaceScope>,
    headers: HeaderMap,
    Json(command): Json<ApplicationMcpCredentialCommand>,
) -> Result<Json<ApplicationMcpCredentialReceipt>, CommandError> {
    let authority_id = command.application_authority_id.trim();
    if authority_id.is_empty() || authority_id.len() > 200 {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "application_authority_id must contain 1 to 200 characters",
        ));
    }
    if command.token.is_empty() || command.token.len() > 16 * 1024 {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "token must contain 1 to 16384 characters",
        ));
    }
    if command.credential_generation == 0 {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "credential_generation must be positive",
        ));
    }
    let idempotency_key = parse_idempotency_key_header(&headers)
        .map_err(|message| error(StatusCode::BAD_REQUEST, "invalid_request_error", message))?
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Idempotency-Key is required",
            )
        })?;
    let (vault_id, source_id, revision, adoption) = state
        .enter_application_mcp_bearer(
            &workspace.0,
            authority_id,
            &command.mcp_server_url,
            &idempotency_key,
            command.credential_generation,
            RedactedString::from(command.token),
        )
        .await
        .map_err(|error_value| match error_value {
            awaken_credential_vault::CredentialError::InvalidSource(_)
            | awaken_credential_vault::CredentialError::MutationConflict(_)
            | awaken_credential_vault::CredentialError::NotActive(_) => error(
                StatusCode::CONFLICT,
                "conflict_error",
                error_value.to_string(),
            ),
            _ => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "application MCP credential storage is unavailable",
            ),
        })?;
    Ok(Json(ApplicationMcpCredentialReceipt {
        object_type: "application_mcp_credential",
        vault_id,
        credential_source_id: source_id.0,
        revision,
        adoption,
    }))
}

/// Awaken Control extension over the canonical Vault aggregate.
pub fn application_mcp_credentials_router(state: Arc<VaultState>) -> Router {
    Router::new()
        .route(
            "/v1/config/application-mcp-credentials",
            post(enter_application_mcp_credential),
        )
        .with_state(state)
}
