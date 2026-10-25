//! The provider byte-custody input of a Session checkpoint. The rest of the
//! Session's sandbox projection is pure and lives in the Session configuration
//! contract (`sandbox_layout`).

/// The Runtime Host is the sole adapter from aggregate checkpoint intent to
/// provider byte-custody input. Ordinary suspension and terminal takeover both
/// call this projection so metadata, expiry, and effect identity cannot drift.
/// Session activity/source/lease coordinates stay on the aggregate-side request;
/// this capture input neither grants authority nor replaces the separate fence.
pub fn provisioning_checkpoint_request(
    request: &awaken_session_state_contract::SandboxCheckpointRequest,
) -> awaken_provisioning_contract::SandboxCheckpointCaptureRequest {
    awaken_provisioning_contract::SandboxCheckpointCaptureRequest {
        workspace_id: request.workspace_id.clone(),
        session_id: request.session_id.to_string(),
        generation_id: request.generation.id().to_owned(),
        environment_fingerprint: request.generation.environment_fingerprint().to_owned(),
        base_image_fingerprint: request.generation.base_image_fingerprint().to_owned(),
        effect_id: request.operation.effect_id().to_owned(),
        format: request.format.clone(),
        created_at_unix_ms: request.created_at_unix_ms,
        expires_at_unix_ms: request.expires_at_unix_ms,
        max_bytes: request.max_bytes,
    }
}
