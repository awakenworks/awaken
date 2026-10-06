//! The protocol-only relay fixture. It is the composition root of its own
//! tests, so it chooses the real clock here.

use std::sync::Arc;

use super::McpRelay;

impl McpRelay {
    /// Protocol-only fixtures have no Session application. Production always
    /// uses for_authority and the existing root-lease decision, never a route clock.
    pub async fn start() -> std::io::Result<Self> {
        Self::start_with_authority_and_exposure(
            Arc::new(|generation, now| {
                awaken_session_realization_contract::realization_lease_is_live_at(
                    generation.lease_expires_at_unix_ms(),
                    now,
                )
            }),
            None,
            awaken_time::system_clock(),
            None,
        )
        .await
    }
}
