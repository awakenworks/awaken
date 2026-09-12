//! The one construction site for rustls-backed HTTP clients.
//!
//! Two facts about every outbound client in this workspace used to be call-site
//! obligations: selecting the process-wide rustls provider, and deciding whether
//! the channel may inherit an ambient `HTTP(S)_PROXY`. Both are now properties of
//! a type. [`InternalHttpClient`] is the private control-plane channel and never
//! inherits an ambient proxy; [`ExternalHttpClient`] reaches third-party
//! endpoints and honours a deliberately configured proxy, except when the target
//! is loopback. Neither exposes its inner transport, and both require a bounded
//! total timeout, so an unbounded or provider-less client cannot be expressed.

use std::time::Duration;

/// Why an outbound HTTP client could not be built or a request could not be
/// completed. Transport faults are opaque strings on purpose: the callers
/// classify by HTTP status, never by transport internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError(String);

impl TransportError {
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransportError {}

/// Default connect budget for a private control-plane hop.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Start a rustls-backed client builder with the process-wide provider already
/// selected. This is the escape hatch for the registered transport authorities
/// that need a knob this crate does not model (a private CA bundle, a streaming
/// body, a relayed method). Ordinary adapters use [`InternalHttpClient`] or
/// [`ExternalHttpClient`] instead.
pub fn rustls_client_builder() -> reqwest::ClientBuilder {
    crate::install_rustls_crypto_provider();
    reqwest::Client::builder()
}

/// Verbs used by the JSON control-plane and admin boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Delete,
}

impl HttpMethod {
    fn as_reqwest(self) -> reqwest::Method {
        match self {
            Self::Get => reqwest::Method::GET,
            Self::Post => reqwest::Method::POST,
            Self::Delete => reqwest::Method::DELETE,
        }
    }
}

/// One JSON request: an absolute URL, an optional bearer credential, extra
/// headers, and an already-serialized body. Payload shaping stays with the
/// adapter that owns the wire contract.
#[derive(Debug, Clone)]
pub struct JsonRequest {
    method: HttpMethod,
    url: String,
    bearer: Option<String>,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

impl JsonRequest {
    #[must_use]
    pub fn new(method: HttpMethod, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            bearer: None,
            headers: Vec::new(),
            body: None,
        }
    }

    #[must_use]
    pub fn get(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Get, url)
    }

    #[must_use]
    pub fn post(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Post, url)
    }

    #[must_use]
    pub fn delete(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Delete, url)
    }

    #[must_use]
    pub fn bearer(mut self, token: &str) -> Self {
        self.bearer = Some(token.to_owned());
        self
    }

    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Attach an already-serialized JSON document.
    #[must_use]
    pub fn json_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }
}

/// A completed response reduced to what the control-plane adapters classify on.
#[derive(Debug, Clone)]
pub struct HttpReply {
    status: u16,
    body: String,
}

impl HttpReply {
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    #[must_use]
    pub fn into_body(self) -> String {
        self.body
    }

    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    #[must_use]
    pub fn is_client_error(&self) -> bool {
        (400..500).contains(&self.status)
    }

    #[must_use]
    pub fn is_server_error(&self) -> bool {
        (500..600).contains(&self.status)
    }

    #[must_use]
    pub fn is_unauthorized(&self) -> bool {
        self.status == 401
    }
}

async fn send_json(
    client: &reqwest::Client,
    request: &JsonRequest,
) -> Result<HttpReply, TransportError> {
    let mut builder = client.request(request.method.as_reqwest(), &request.url);
    if let Some(bearer) = &request.bearer {
        builder = builder.bearer_auth(bearer);
    }
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    if let Some(body) = &request.body {
        builder = builder
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.clone());
    }
    let response = builder
        .send()
        .await
        .map_err(|error| TransportError(error.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|error| TransportError(error.to_string()))?;
    Ok(HttpReply { status, body })
}

/// Private control-plane client. Never inherits an ambient egress proxy: this
/// traffic stays inside the deployment's own network, and a workstation or
/// sidecar proxy variable must not silently redirect it.
#[derive(Clone)]
pub struct InternalHttpClient(reqwest::Client);

impl InternalHttpClient {
    /// Build with the default connect budget and an explicit total deadline.
    pub fn new(timeout: Duration) -> Result<Self, TransportError> {
        Self::with_connect_timeout(DEFAULT_CONNECT_TIMEOUT, timeout)
    }

    pub fn with_connect_timeout(
        connect_timeout: Duration,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        Self::build(connect_timeout, timeout, None)
    }

    /// Build a client that additionally trusts a deployment-private CA bundle.
    /// An empty or unparsable bundle fails closed rather than silently falling
    /// back to the public roots.
    pub fn with_private_ca(
        connect_timeout: Duration,
        timeout: Duration,
        ca_pem: &[u8],
    ) -> Result<Self, TransportError> {
        Self::build(connect_timeout, timeout, Some(ca_pem))
    }

    fn build(
        connect_timeout: Duration,
        timeout: Duration,
        ca_pem: Option<&[u8]>,
    ) -> Result<Self, TransportError> {
        let mut builder = rustls_client_builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .timeout(timeout);
        if let Some(ca_pem) = ca_pem {
            // Rustls-backed `from_pem_bundle` merely stores the bytes; an empty
            // iteration also succeeds when building the client. A configured
            // private CA must actually add at least one validated certificate.
            let certificates = reqwest::Certificate::from_pem_bundle(ca_pem)
                .map_err(|_| TransportError("invalid private CA certificate encoding".into()))?;
            if certificates.is_empty() {
                return Err(TransportError(
                    "private CA bundle contains no certificates".into(),
                ));
            }
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        builder
            .build()
            .map(Self)
            .map_err(|error| TransportError(error.to_string()))
    }

    pub async fn send(&self, request: &JsonRequest) -> Result<HttpReply, TransportError> {
        send_json(&self.0, request).await
    }
}

impl std::fmt::Debug for InternalHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InternalHttpClient")
    }
}

/// Third-party egress client. A deliberately configured corporate proxy is
/// honoured, because that is how these deployments reach the public internet —
/// except for a loopback target, where an ambient proxy would break an
/// on-host emulator or sidecar that is not reachable through it.
#[derive(Clone)]
pub struct ExternalHttpClient(reqwest::Client);

impl ExternalHttpClient {
    /// Build the client for one target URL, applying the loopback proxy rule.
    pub fn for_target(url: &str, timeout: Duration) -> Result<Self, TransportError> {
        let mut builder = rustls_client_builder().timeout(timeout);
        if target_is_loopback(url) {
            builder = builder.no_proxy();
        }
        builder
            .build()
            .map(Self)
            .map_err(|error| TransportError(error.to_string()))
    }

    pub async fn send(&self, request: &JsonRequest) -> Result<HttpReply, TransportError> {
        send_json(&self.0, request).await
    }
}

impl std::fmt::Debug for ExternalHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExternalHttpClient")
    }
}

/// `localhost` or a loopback IP literal, the two shapes an on-host emulator or
/// sidecar takes. An unparsable URL is treated as remote.
#[must_use]
pub fn target_is_loopback(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .is_some_and(|host| {
            // `Url::host_str` preserves the brackets around an IPv6 literal;
            // remove only those parser-validated delimiters before classifying.
            let host = host
                .strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .unwrap_or(&host);
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

/// Why a configured service base URL was rejected before any request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseUrlRejected {
    Malformed,
    NotHttp,
    CarriesQueryOrFragment,
}

impl std::fmt::Display for BaseUrlRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "base url is malformed",
            Self::NotHttp => "base url must use http or https",
            Self::CarriesQueryOrFragment => "base url must not carry a query or fragment",
        })
    }
}

impl std::error::Error for BaseUrlRejected {}

/// Validate a configured service base URL and return it without a trailing
/// slash, so every adapter concatenates paths against the same normal form.
pub fn normalize_http_base_url(base_url: &str) -> Result<String, BaseUrlRejected> {
    let trimmed = base_url.trim_end_matches('/');
    let parsed = reqwest::Url::parse(trimmed).map_err(|_| BaseUrlRejected::Malformed)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(BaseUrlRejected::NotHttp);
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(BaseUrlRejected::CarriesQueryOrFragment);
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_owned_client_constructor_selects_the_process_provider() {
        // Cause/effect decision table: C1 the workspace feature graph compiles
        // more than one rustls provider; C2 an adapter builds a client through
        // this authority. R1 C1+C2 => a process-wide provider is installed
        // before the client exists, so no later rustls consumer (kube's
        // `ClientConfig::builder`) can observe an unset default.
        let _internal = InternalHttpClient::new(Duration::from_secs(1)).expect("R1");
        let _external =
            ExternalHttpClient::for_target("https://example.com", Duration::from_secs(1))
                .expect("R1");
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "R1"
        );
    }

    #[test]
    fn loopback_targets_refuse_an_ambient_proxy_and_remote_targets_keep_it() {
        // R1 `localhost`/loopback literal => the ambient proxy is refused,
        // because an on-host emulator is not reachable through it; R2 a routable
        // host => the deliberately configured proxy stays in force; R3 an
        // unparsable URL classifies as remote rather than silently loopback.
        for url in [
            "http://localhost:8080/v1",
            "http://127.0.0.1:9/v1",
            "https://LOCALHOST/v1",
            "http://[::1]:8080/v1",
        ] {
            assert!(target_is_loopback(url), "R1 {url}");
        }
        for url in ["https://cloud.example.com/v1", "http://10.0.0.5/v1"] {
            assert!(!target_is_loopback(url), "R2 {url}");
        }
        assert!(!target_is_loopback("not a url"), "R3");
    }

    #[test]
    fn a_private_ca_must_contribute_at_least_one_certificate() {
        // R1 an empty or unparsable bundle fails closed; a client that silently
        // fell back to the public roots would trust the wrong issuer.
        assert!(
            InternalHttpClient::with_private_ca(
                DEFAULT_CONNECT_TIMEOUT,
                Duration::from_secs(1),
                b"",
            )
            .is_err(),
            "R1 empty"
        );
        assert!(
            InternalHttpClient::with_private_ca(
                DEFAULT_CONNECT_TIMEOUT,
                Duration::from_secs(1),
                b"-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n",
            )
            .is_err(),
            "R1 unparsable"
        );
    }

    #[test]
    fn base_url_admission_is_total_over_scheme_and_url_parts() {
        // R1 http(s) without query/fragment => admitted in trailing-slash-free
        // normal form; R2 another scheme => rejected; R3 a query or fragment =>
        // rejected, because path concatenation would silently corrupt it.
        assert_eq!(
            normalize_http_base_url("https://control.example.com/"),
            Ok("https://control.example.com".to_owned()),
            "R1"
        );
        assert_eq!(
            normalize_http_base_url("http://coordinator:8080"),
            Ok("http://coordinator:8080".to_owned()),
            "R1"
        );
        assert_eq!(
            normalize_http_base_url("file:///etc/passwd"),
            Err(BaseUrlRejected::NotHttp),
            "R2"
        );
        assert_eq!(
            normalize_http_base_url("not a url"),
            Err(BaseUrlRejected::Malformed),
            "R2"
        );
        assert_eq!(
            normalize_http_base_url("https://control.example.com/?a=b"),
            Err(BaseUrlRejected::CarriesQueryOrFragment),
            "R3"
        );
        assert_eq!(
            normalize_http_base_url("https://control.example.com/#f"),
            Err(BaseUrlRejected::CarriesQueryOrFragment),
            "R3"
        );
    }

    #[tokio::test]
    async fn a_json_request_carries_verb_credential_headers_and_body_and_returns_the_reply() {
        // R1 the built request reaches the server with its verb, bearer, extra
        // header, JSON content type and body; R2 the reply is reduced to a
        // status plus body that classify without naming the transport crate;
        // R3 a request whose deadline elapses fails as a transport error rather
        // than hanging, which is the property `Client::new()` used to lose.
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let app = axum_probe_server(observed);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = app(listener).await;
        });

        let client = InternalHttpClient::new(Duration::from_secs(5)).unwrap();
        let reply = client
            .send(
                &JsonRequest::post(format!("http://{address}/v1/thing"))
                    .bearer("token-a")
                    .header("x-awaken-probe", "yes")
                    .json_body("{\"a\":1}"),
            )
            .await
            .expect("R1");
        assert_eq!(reply.status(), 202, "R2");
        assert!(reply.is_success() && !reply.is_client_error(), "R2");
        assert_eq!(reply.body(), "{\"ok\":true}", "R2");
        let seen = seen.lock().unwrap().clone();
        assert!(seen.contains(&"POST".to_string()), "R1 verb");
        assert!(seen.contains(&"Bearer token-a".to_string()), "R1 bearer");
        assert!(seen.contains(&"yes".to_string()), "R1 header");
        assert!(seen.contains(&"application/json".to_string()), "R1 type");
        assert!(seen.contains(&"{\"a\":1}".to_string()), "R1 body");

        let impatient = InternalHttpClient::new(Duration::from_millis(20)).unwrap();
        let stalled = impatient
            .send(&JsonRequest::get(format!("http://{address}/stall")))
            .await;
        assert!(stalled.is_err(), "R3");
    }

    fn axum_probe_server(
        observed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> impl FnOnce(
        tokio::net::TcpListener,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>,
    > {
        move |listener| {
            Box::pin(async move {
                loop {
                    let (stream, _) = listener.accept().await?;
                    let observed = observed.clone();
                    tokio::spawn(async move { serve_probe(stream, observed).await });
                }
            })
        }
    }

    async fn serve_probe(
        mut stream: tokio::net::TcpStream,
        observed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut buffer = vec![0_u8; 4096];
        let read = match stream.read(&mut buffer).await {
            Ok(read) => read,
            Err(_) => return,
        };
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();
        if request.starts_with("GET /stall") {
            tokio::time::sleep(Duration::from_secs(5)).await;
            return;
        }
        {
            let mut observed = observed.lock().unwrap();
            for line in request.lines() {
                if let Some(method) = line.split_whitespace().next()
                    && ["GET", "POST", "DELETE"].contains(&method)
                {
                    observed.push(method.to_owned());
                }
                if let Some((_, value)) = line.split_once(": ") {
                    observed.push(value.trim().to_owned());
                }
            }
            if let Some((_, body)) = request.split_once("\r\n\r\n") {
                observed.push(body.to_owned());
            }
        }
        let body = "{\"ok\":true}";
        let response = format!(
            "HTTP/1.1 202 Accepted\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    }
}
