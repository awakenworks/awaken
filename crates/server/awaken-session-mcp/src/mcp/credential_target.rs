// @formal entrypoint credential_effect.mcp_realization_preserves_exact_authoring_target
//! The exact credential realization target of one admitted MCP stage request.

/// Select the exact target carried by the admitted MCP realization request.
/// Keeping this identity projection explicit prevents credential materialization
/// from silently rebinding the request to a name/target tuple or another derived
/// lookup key.
#[must_use]
pub(crate) fn exact_credential_realization_target<T>(request_target: &T) -> &T {
    request_target
}

#[cfg(kani)]
/// @obligation credential_effect.mcp_realization_preserves_exact_authoring_target
#[kani::proof]
fn mcp_credential_realization_preserves_the_request_target_exactly() {
    let request_target: u64 = kani::any();
    let selected = exact_credential_realization_target(&request_target);
    assert_eq!(*selected, request_target);
    assert!(std::ptr::eq(selected, &request_target));
}

#[cfg(test)]
mod tests {
    use super::exact_credential_realization_target;

    #[test]
    fn materialization_target_is_the_original_typed_request_target() {
        let target = awaken_session_mcp_contract::McpTarget::parse_http(
            "https://credential-bound.example.test/mcp?tenant=exact",
        )
        .expect("valid target");
        let selected = exact_credential_realization_target(&target);
        assert!(std::ptr::eq(selected, &target));
        assert_eq!(selected, &target);
        assert_eq!(
            selected.http_url(),
            Some("https://credential-bound.example.test/mcp?tenant=exact")
        );
    }
}
