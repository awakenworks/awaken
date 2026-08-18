//! HTTP credential extraction rules shared by the management PEPs.

use axum::http::HeaderMap;

/// The presented credential: `Authorization: Bearer <token>` (the documented
/// form) or `x-api-key: <token>` (what the Anthropic SDK sends for `apiKey`).
pub(super) fn bearer_token(headers: &HeaderMap) -> Option<String> {
    if headers.contains_key(axum::http::header::AUTHORIZATION) {
        // An explicit Authorization header always wins, including when it is
        // malformed. Falling back to x-api-key would turn a failed stronger
        // credential into an unintended successful weaker credential.
        return authorization_bearer_token(headers);
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
}

pub(super) fn authorization_bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let mut parts = value.splitn(2, ' ');
    match (parts.next(), parts.next()) {
        (Some(scheme), Some(token))
            if scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty() =>
        {
            Some(token.trim().to_string())
        }
        _ => None,
    }
}

pub(super) fn is_tunnel_route(path: &str) -> bool {
    super::in_family(path, "/v1/tunnels")
}

pub(super) fn is_legacy_tunnel_route(path: &str) -> bool {
    super::in_family(path, "/v1/organizations/tunnels")
}

/// Extract the plain management workspace selector. Exotic percent-encoded
/// identifiers intentionally fail the equality fence in the closed direction.
pub(super) fn query_workspace_id(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "workspace_id")
        .map(|(_, value)| value.replace('+', " "))
}
