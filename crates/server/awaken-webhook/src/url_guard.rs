//! SSRF guard for subscription endpoint URLs.
//!
//! A subscription URL is fetched server-side by the dispatcher, so an unvalidated
//! URL is a server-side request forgery vector: a caller could point a webhook at
//! the cloud metadata endpoint (`169.254.169.254`), a loopback admin port, or an
//! internal service. This is a URL-shape guard — it requires `https` and rejects a
//! host that is an IP literal in a non-global range (or `localhost`). It does not
//! resolve DNS (a name that resolves to an internal IP, i.e. DNS-rebinding, is a
//! deeper problem handled by resolve-and-pin at request time, not here); the aim is
//! to close the obvious literal-target holes at admission.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Why an endpoint URL was rejected at admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlRejected {
    /// The scheme was not `https` (plain `http`, `file:`, `javascript:`, …).
    NotHttps,
    /// The URL had no scheme/authority we could parse.
    Malformed,
    /// The host is an IP literal in a non-global range, or `localhost` — an
    /// internal/loopback/link-local target (SSRF).
    PrivateHost,
}

impl std::fmt::Display for UrlRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            UrlRejected::NotHttps => "endpoint url must use https",
            UrlRejected::Malformed => "endpoint url is malformed",
            UrlRejected::PrivateHost => "endpoint url points at a private or loopback host",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for UrlRejected {}

/// Validate a subscription endpoint URL: `https` scheme, and a host that is not an
/// internal/loopback/link-local IP literal (nor `localhost`). Public DNS hosts pass.
pub fn validate_endpoint_url(url: &str) -> Result<(), UrlRejected> {
    let (scheme, rest) = url.split_once("://").ok_or(UrlRejected::Malformed)?;
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(UrlRejected::NotHttps);
    }
    // The authority is everything up to the path/query/fragment.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Drop any `user:pass@` userinfo.
    let hostport = authority
        .rsplit_once('@')
        .map_or(authority, |(_, after)| after);
    let host = host_of(hostport)?;
    if host.is_empty() {
        return Err(UrlRejected::Malformed);
    }
    if host.eq_ignore_ascii_case("localhost") {
        return Err(UrlRejected::PrivateHost);
    }
    // An IP-literal host must be globally routable; a DNS name passes this guard.
    if let Ok(ip) = host.parse::<IpAddr>()
        && !is_global(&ip)
    {
        return Err(UrlRejected::PrivateHost);
    }
    Ok(())
}

/// The host from an `authority` (`host`, `host:port`, `[v6]`, or `[v6]:port`),
/// with the brackets stripped from an IPv6 literal.
fn host_of(hostport: &str) -> Result<&str, UrlRejected> {
    if let Some(after) = hostport.strip_prefix('[') {
        // `[v6]` or `[v6]:port` — the host is up to the closing bracket.
        return after
            .split_once(']')
            .map(|(h, _)| h)
            .ok_or(UrlRejected::Malformed);
    }
    // `host` or `host:port` — a bare v4/name never contains ':' except as the port.
    Ok(hostport.split(':').next().unwrap_or(hostport))
}

/// Whether an IP literal is safe to fetch from the server (globally routable), i.e.
/// not loopback / private / link-local / unique-local / unspecified / multicast /
/// documentation. The cloud metadata address `169.254.169.254` is link-local and so
/// is rejected here.
fn is_global(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_global_v4(v4),
        IpAddr::V6(v6) => is_global_v6(v6),
    }
}

fn is_global_v4(v4: &Ipv4Addr) -> bool {
    // 100.64.0.0/10 (CGNAT shared address space) is carrier-internal; treat as private.
    let cgnat = v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40;
    !(v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || cgnat)
}

fn is_global_v6(v6: &Ipv6Addr) -> bool {
    // An IPv4-mapped address (`::ffff:a.b.c.d`) is really a v4 target — check that.
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_global_v4(&mapped);
    }
    let seg0 = v6.segments()[0];
    let unique_local = (seg0 & 0xfe00) == 0xfc00; // fc00::/7
    let link_local = (seg0 & 0xffc0) == 0xfe80; // fe80::/10
    !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || unique_local || link_local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_https_url_is_accepted() {
        for url in [
            "https://hooks.example.com/ingest",
            "https://x.example/y?z=1",
            "https://api.example.com:8443/webhook",
            "https://[2606:4700:4700::1111]/hook", // a public IPv6 literal
        ] {
            assert_eq!(validate_endpoint_url(url), Ok(()), "{url} should pass");
        }
    }

    #[test]
    fn a_non_https_scheme_is_rejected() {
        for url in [
            "http://hooks.example.com/x",
            "file:///etc/passwd",
            "javascript:alert(1)//",
            "ftp://example.com/x",
        ] {
            assert!(
                matches!(
                    validate_endpoint_url(url),
                    Err(UrlRejected::NotHttps | UrlRejected::Malformed)
                ),
                "{url} must not pass"
            );
        }
    }

    #[test]
    fn the_cloud_metadata_and_loopback_targets_are_rejected() {
        for url in [
            "https://169.254.169.254/latest/meta-data/", // AWS/GCP metadata (link-local)
            "https://127.0.0.1/admin",                   // loopback
            "https://localhost/admin",                   // loopback by name
            "https://10.0.0.5/internal",                 // private
            "https://192.168.1.1/router",                // private
            "https://172.16.0.9/svc",                    // private
            "https://[::1]/x",                           // IPv6 loopback
            "https://[fd00::1]/x",                       // IPv6 unique-local
            "https://[fe80::1]/x",                       // IPv6 link-local
            "https://0.0.0.0/x",                         // unspecified
            "https://100.100.0.1/x",                     // CGNAT shared range
            "https://[::ffff:127.0.0.1]/x",              // IPv4-mapped loopback
        ] {
            assert_eq!(
                validate_endpoint_url(url),
                Err(UrlRejected::PrivateHost),
                "{url} is an SSRF target and must be rejected"
            );
        }
    }

    #[test]
    fn a_malformed_url_is_rejected() {
        for url in ["not a url", "https://", "https:///path"] {
            assert!(
                matches!(
                    validate_endpoint_url(url),
                    Err(UrlRejected::Malformed | UrlRejected::PrivateHost)
                ),
                "{url:?} must not pass"
            );
        }
    }
}
