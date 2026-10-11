// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: Send admission produces neutral error facts before any Run effect;
// @formal reason: HTTP/RPC version projections remain owned by the existing adapter.
//! Shared send configuration admission for A2A bindings.

use crate::types::SendMessageRequest;
use awaken_session_application_contract::RunApplicationError;

pub(crate) fn validate_send_configuration(
    req: &SendMessageRequest,
) -> Result<(), RunApplicationError> {
    let Some(configuration) = &req.configuration else {
        return Ok(());
    };
    if !configuration.accepted_output_modes.is_empty()
        && !configuration.accepted_output_modes.iter().any(|mode| {
            matches!(
                mode.as_str(),
                "text/plain" | "text/markdown" | "application/json"
            )
        })
    {
        return Err(RunApplicationError::bad_request_classified(
            "unsupported_output_modes",
            format!(
                "unsupported output modes: {}",
                configuration.accepted_output_modes.join(", ")
            ),
        ));
    }
    Ok(())
}
