//! Shared safe outbound HTTP boundary for server-side callbacks.
//!
//! Admission requires HTTPS and rejects private IP literals. Every production
//! request resolves and pins only globally-routable addresses and never follows
//! redirects, closing DNS-rebinding and redirect-based SSRF paths. Protocol and
//! webhook adapters own payloads/retries; this crate owns only transport safety
//! and the common HTTP retry classification.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;

/// Why a server-side callback URL was rejected at admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlRejected {
    NotHttps,
    Malformed,
    PrivateHost,
}

impl std::fmt::Display for UrlRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotHttps => "endpoint url must use https",
            Self::Malformed => "endpoint url is malformed",
            Self::PrivateHost => "endpoint url points at a private or loopback host",
        })
    }
}

impl std::error::Error for UrlRejected {}

/// Validate the stable URL shape before it is stored. DNS is intentionally
/// checked again and pinned at request time by [`GuardedHttpSender`].
pub fn validate_endpoint_url(url: &str) -> Result<(), UrlRejected> {
    let parsed = reqwest::Url::parse(url).map_err(|_| UrlRejected::Malformed)?;
    if parsed.scheme() != "https" {
        return Err(UrlRejected::NotHttps);
    }
    let host = parsed.host_str().ok_or(UrlRejected::Malformed)?;
    // `Url::host_str` preserves brackets around an IPv6 literal; remove only
    // those parser-validated delimiters before classifying the address.
    let dns_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .trim_end_matches('.');
    if dns_host.eq_ignore_ascii_case("localhost")
        || dns_host.to_ascii_lowercase().ends_with(".localhost")
    {
        return Err(UrlRejected::PrivateHost);
    }
    if let Ok(ip) = dns_host.parse::<IpAddr>()
        && !ip_is_global(&ip)
    {
        return Err(UrlRejected::PrivateHost);
    }
    Ok(())
}

fn ip_is_global(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_global_v4(v4),
        IpAddr::V6(v6) => is_global_v6(v6),
    }
}

fn is_global_v4(v4: &Ipv4Addr) -> bool {
    let [a, b, c, d] = v4.octets();
    let this_network = a == 0;
    let cgnat = a == 100 && (b & 0xc0) == 0x40;
    let protocol_assignments = a == 192 && b == 0 && c == 0 && !matches!(d, 9 | 10);
    let benchmarking = a == 198 && matches!(b, 18 | 19);
    let reserved = a >= 240;
    !(v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || this_network
        || cgnat
        || protocol_assignments
        || benchmarking
        || reserved)
}

fn is_global_v6(v6: &Ipv6Addr) -> bool {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_global_v4(&mapped);
    }
    let segment = v6.segments()[0];
    let unique_local = (segment & 0xfe00) == 0xfc00;
    let link_local = (segment & 0xffc0) == 0xfe80;
    let documentation = matches!(v6.segments(), [0x2001, 0x0db8, ..] | [0x3fff, ..]);
    let six_to_four = segment == 0x2002;
    let segment_routing = segment == 0x5f00;
    !(v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || unique_local
        || link_local
        || documentation
        || six_to_four
        || segment_routing)
}

/// HTTP responses worth retrying. Transport errors are retryable separately.
#[must_use]
pub fn status_is_retryable(code: u16) -> bool {
    matches!(code, 408 | 425 | 429) || (500..600).contains(&code)
}

#[async_trait]
pub trait HttpSender: Send + Sync {
    async fn post(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
        body: String,
    ) -> Result<u16, String>;
}

/// HTTPS sender with delivery-time resolve-and-pin SSRF protection.
pub struct GuardedHttpSender {
    client: reqwest::Client,
    guard: bool,
    timeout: Duration,
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

impl GuardedHttpSender {
    #[must_use]
    pub fn guarded() -> Self {
        Self::build(DEFAULT_TIMEOUT, true)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self::build(timeout, false)
    }

    fn build(timeout: Duration, guard: bool) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(timeout)
                .build()
                .expect("outbound HTTP client builds"),
            guard,
            timeout,
        }
    }

    async fn pinned_client(&self, url: &str) -> Result<reqwest::Client, String> {
        let parsed = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
        if parsed.scheme() != "https" {
            return Err("endpoint must use https".into());
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| "endpoint has no host".to_string())?
            .to_string();
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addresses = resolve_global(&host, port).await?;
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.timeout)
            .resolve_to_addrs(&host, &addresses)
            .build()
            .map_err(|error| error.to_string())
    }
}

async fn resolve_global(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !ip_is_global(&ip) {
            return Err(format!("endpoint {host} is a private/loopback address"));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let safe = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| error.to_string())?
        .filter(|address| ip_is_global(&address.ip()))
        .collect::<Vec<_>>();
    if safe.is_empty() {
        Err(format!(
            "endpoint {host} resolves only to private/loopback addresses"
        ))
    } else {
        Ok(safe)
    }
}

#[async_trait]
impl HttpSender for GuardedHttpSender {
    async fn post(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
        body: String,
    ) -> Result<u16, String> {
        let client = if self.guard {
            self.pinned_client(url).await?
        } else {
            self.client.clone()
        };
        let mut request = client.post(url).body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request
            .send()
            .await
            .map(|response| response.status().as_u16())
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_covers_scheme_authority_and_private_host_partition() {
        // Cause/effect table: R1 HTTPS+public => admit; R2 non-HTTPS/malformed
        // => reject; R3 HTTPS+private/loopback/metadata => reject. Userinfo is
        // removed before the real host is classified.
        for url in [
            "https://hooks.example.com/ingest",
            "https://api.example.com:8443/hook",
            "https://[2606:4700:4700::1111]/hook",
            "https://user:pass@hooks.example.com/ingest",
        ] {
            assert_eq!(validate_endpoint_url(url), Ok(()), "{url}");
        }
        for url in [
            "http://example.com",
            "file:///etc/passwd",
            "not a url",
            "https://example.com:bad-port/hook",
            "https://[::1/hook",
        ] {
            assert!(validate_endpoint_url(url).is_err(), "{url}");
        }
        for url in [
            "https://127.0.0.1/admin",
            "https://localhost/admin",
            "https://169.254.169.254/latest/meta-data",
            "https://10.0.0.5/internal",
            "https://[::1]/x",
            "https://[fd00::1]/x",
            "https://[::ffff:127.0.0.1]/x",
            "https://service.localhost/x",
            "https://0.1.2.3/x",
            "https://198.18.0.1/x",
            "https://[2001:db8::1]/x",
            "https://public.example.com@127.0.0.1/steal",
        ] {
            assert_eq!(
                validate_endpoint_url(url),
                Err(UrlRejected::PrivateHost),
                "{url}"
            );
        }
    }

    #[test]
    fn status_partition_is_total_at_retry_boundaries() {
        // R1 timeout/early/rate-limit/server => retry; R2 every other status =>
        // terminal. Success is classified by the caller before this predicate.
        for status in [408, 425, 429, 500, 599] {
            assert!(status_is_retryable(status), "{status}");
        }
        for status in [200, 299, 300, 400, 404, 422, 600] {
            assert!(!status_is_retryable(status), "{status}");
        }
    }

    #[tokio::test]
    async fn delivery_time_guard_rejects_private_resolution_and_non_https() {
        // R1 public literal resolves to itself; R2 private literal/name produces
        // no connect target; R3 non-HTTPS is rejected before a request.
        assert_eq!(
            resolve_global("1.1.1.1", 443).await.unwrap(),
            vec!["1.1.1.1:443".parse::<SocketAddr>().unwrap()]
        );
        for host in ["127.0.0.1", "169.254.169.254", "10.0.0.5", "localhost"] {
            assert!(resolve_global(host, 443).await.is_err(), "{host}");
        }
        let sender = GuardedHttpSender::guarded();
        assert!(
            sender
                .post("https://127.0.0.1:9/hook", vec![], "{}".into())
                .await
                .is_err()
        );
        assert!(
            sender
                .post("http://example.com/hook", vec![], "{}".into())
                .await
                .is_err()
        );
    }
}
