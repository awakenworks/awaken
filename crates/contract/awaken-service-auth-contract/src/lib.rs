//! Authentication primitives shared by private service-to-service adapters.
//!
//! This leaf owns no HTTP framework, identity store, authorization policy, or
//! domain command. Adapters pass raw header bytes after transport parsing.

/// Match the exact private Bearer credential projected into both sides of one
/// service boundary. Scheme matching is deliberately case-sensitive and the
/// function fails closed for a missing or malformed header.
#[must_use]
pub fn service_bearer_token_matches(authorization: Option<&[u8]>, expected: &str) -> bool {
    authorization
        .and_then(|value| value.strip_prefix(b"Bearer "))
        .is_some_and(|actual| actual == expected.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_bearer_matching_fails_closed() {
        // Cause/effect decision table: R1 exact scheme and bytes -> authorize;
        // R2 missing header, R3 wrong scheme, R4 wrong token -> deny. These four
        // rules cover both parsing branches and both equality outcomes.
        assert!(
            service_bearer_token_matches(Some(b"Bearer secret"), "secret"),
            "R1"
        );
        assert!(!service_bearer_token_matches(None, "secret"), "R2");
        assert!(
            !service_bearer_token_matches(Some(b"bearer secret"), "secret"),
            "R3"
        );
        assert!(
            !service_bearer_token_matches(Some(b"Bearer other"), "secret"),
            "R4"
        );
    }
}
