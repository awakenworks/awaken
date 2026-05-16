//! Shared HTTP authentication helpers.

/// Strip the `Bearer` scheme word from an `Authorization` header value
/// (ASCII case-insensitive, per RFC 6750 §2.1), returning the remainder
/// after the first ASCII space.
///
/// Any additional whitespace in the token is preserved — trimming policy
/// belongs to the caller. Returns `None` if the value does not contain a
/// space, or the leading word is not `bearer`.
pub(crate) fn strip_bearer_prefix(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::strip_bearer_prefix;

    #[test]
    fn accepts_canonical_and_mixed_case_schemes() {
        assert_eq!(strip_bearer_prefix("Bearer abc"), Some("abc"));
        assert_eq!(strip_bearer_prefix("bearer abc"), Some("abc"));
        assert_eq!(strip_bearer_prefix("BEARER abc"), Some("abc"));
        assert_eq!(strip_bearer_prefix("bEaReR abc"), Some("abc"));
    }

    #[test]
    fn rejects_non_bearer_or_missing_space() {
        assert_eq!(strip_bearer_prefix("Basic abc"), None);
        assert_eq!(strip_bearer_prefix("Bearerabc"), None);
        assert_eq!(strip_bearer_prefix(""), None);
        assert_eq!(strip_bearer_prefix("Bearer"), None);
    }

    #[test]
    fn does_not_trim_extra_token_whitespace() {
        // Only the first ASCII space is consumed as the scheme/token
        // delimiter. Any further whitespace is part of the token and is
        // returned verbatim — trimming policy belongs to the caller.
        let double_space = "Bearer \u{0020}abc"; // explicit two ASCII spaces
        assert_eq!(strip_bearer_prefix(double_space), Some(" abc"));
        assert_eq!(strip_bearer_prefix("Bearer abc "), Some("abc "));
        assert_eq!(strip_bearer_prefix("Bearer \tabc"), Some("\tabc"));
    }
}
