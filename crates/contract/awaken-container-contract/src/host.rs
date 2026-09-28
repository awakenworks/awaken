//! Allowlist host normalization shared by container egress and the Session
//! owner's relay admission.

/// Canonical form of one `NetworkPolicy::Allowlist` host: trimmed, without a
/// trailing dot, ASCII-lowercased, and a syntactically valid DNS name (IP
/// literals are not hosts). `None` for anything else.
///
/// Consumers: the container egress planner (capability issuance) and the Runtime
/// Host's MCP relay admission, which must compare the same normalized names.
/// Moved here from `awaken-sandbox-container` so both read one rule.
#[must_use]
pub fn normalize_allowlist_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || !host.is_ascii()
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.len() > 253
    {
        return None;
    }
    host.split('.')
        .all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        .then_some(host)
}

#[cfg(test)]
mod tests {
    use super::normalize_allowlist_host;

    #[test]
    fn allowlist_hosts_normalize_to_one_canonical_dns_name() {
        // Decision table: H1 case/space/trailing-dot variants of a DNS name =>
        // one lowercase name; H2 empty, IP literal, non-ASCII, bad label => None.
        for (raw, expected) in [
            (" API.Example.COM. ", Some("api.example.com")),
            ("a-b.example", Some("a-b.example")),
            ("", None),
            ("10.0.0.1", None),
            ("::1", None),
            ("bücher.example", None),
            ("-bad.example", None),
            ("bad-.example", None),
            ("a..example", None),
        ] {
            assert_eq!(
                normalize_allowlist_host(raw).as_deref(),
                expected,
                "{raw:?}"
            );
        }
    }
}
