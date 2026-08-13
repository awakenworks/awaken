//! Canonical public Managed API header vocabulary.

/// The Managed Agents beta wire header accepted by every Managed endpoint.
pub const MANAGED_BETA: &str = "managed-agents-2026-04-01";
/// Official beta gate for the sibling User Profiles API family.
pub const USER_PROFILES_BETA: &str = "user-profiles-2026-03-24";
/// Research-preview MCP Tunnel API beta.
pub const TUNNELS_BETA: &str = "mcp-tunnels-2026-06-22";

/// Parse the shared mutation replay header once for Managed and Awaken-owned
/// control commands. The caller owns its protocol-specific error envelope.
pub fn parse_idempotency_key_header(
    headers: &axum::http::HeaderMap,
) -> Result<Option<String>, &'static str> {
    let key = headers
        .get("idempotency-key")
        .map(|value| {
            value
                .to_str()
                .map(str::to_string)
                .map_err(|_| "Idempotency-Key must be visible ASCII")
        })
        .transpose()?;
    if key
        .as_ref()
        .is_some_and(|key| key.trim().is_empty() || key.len() > 255)
    {
        return Err("Idempotency-Key must contain 1 to 255 characters");
    }
    Ok(key)
}
