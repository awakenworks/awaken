//! OTel environment-variable configuration.
//!
//! Parses the standard `OTEL_EXPORTER_OTLP_*` variables into a typed struct, per the
//! [OpenTelemetry exporter spec](https://opentelemetry.io/docs/specs/otel/protocol/exporter/).
//! Parsing is pure (`from_env` / builder) so it stays testable under the workspace's
//! `unsafe_code = "forbid"` lint, which forbids mutating the environment in tests.

use std::convert::Infallible;
use std::str::FromStr;
use std::time::Duration;

/// Protocol used by the OTLP exporter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum OtelProtocol {
    Grpc,
    #[default]
    HttpProtobuf,
    HttpJson,
}

impl FromStr for OtelProtocol {
    type Err = Infallible;

    /// Parse a protocol string per the OTel spec. Recognised: `grpc`,
    /// `http/protobuf`, `http/json`; anything else falls back to the default.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_lowercase().as_str() {
            "grpc" => Self::Grpc,
            "http/protobuf" => Self::HttpProtobuf,
            "http/json" => Self::HttpJson,
            _ => Self::default(),
        })
    }
}

/// Configuration parsed from `OTEL_EXPORTER_OTLP_*` environment variables.
#[derive(Debug, Clone)]
pub struct OtelConfig {
    /// Base OTLP endpoint (`OTEL_EXPORTER_OTLP_ENDPOINT`).
    pub endpoint: Option<String>,
    /// Signal-specific traces endpoint (`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`).
    pub traces_endpoint: Option<String>,
    /// Base OTLP protocol (`OTEL_EXPORTER_OTLP_PROTOCOL`).
    pub protocol: OtelProtocol,
    /// Signal-specific traces protocol (`OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`).
    pub traces_protocol: Option<OtelProtocol>,
    /// Extra headers sent with every request (`OTEL_EXPORTER_OTLP_HEADERS`).
    pub headers: Vec<(String, String)>,
    /// Export timeout (`OTEL_EXPORTER_OTLP_TIMEOUT`, default 10 s).
    pub timeout: Duration,
    /// Logical service name (`OTEL_SERVICE_NAME`).
    pub service_name: Option<String>,
    /// Service version (`OTEL_SERVICE_VERSION`).
    pub service_version: Option<String>,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            traces_endpoint: None,
            protocol: OtelProtocol::default(),
            traces_protocol: None,
            headers: Vec::new(),
            timeout: Duration::from_secs(10),
            service_name: None,
            service_version: None,
        }
    }
}

impl OtelConfig {
    /// Parse configuration from environment variables.
    pub fn from_env() -> Self {
        Self {
            endpoint: std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
            traces_endpoint: std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").ok(),
            protocol: std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL")
                .ok()
                .map(|s| s.parse::<OtelProtocol>().unwrap_or_default())
                .unwrap_or_default(),
            traces_protocol: std::env::var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
                .ok()
                .map(|s| s.parse::<OtelProtocol>().unwrap_or_default()),
            headers: parse_headers(
                &std::env::var("OTEL_EXPORTER_OTLP_HEADERS").unwrap_or_default(),
            ),
            timeout: std::env::var("OTEL_EXPORTER_OTLP_TIMEOUT")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .map(Duration::from_millis)
                .unwrap_or(Duration::from_secs(10)),
            service_name: std::env::var("OTEL_SERVICE_NAME").ok(),
            service_version: std::env::var("OTEL_SERVICE_VERSION").ok(),
        }
    }

    /// Create a builder for programmatic construction.
    pub fn builder() -> OtelConfigBuilder {
        OtelConfigBuilder::default()
    }

    /// Returns `true` when at least one endpoint is configured.
    pub fn is_configured(&self) -> bool {
        self.endpoint.is_some() || self.traces_endpoint.is_some()
    }

    /// Resolve the effective traces endpoint (signal-specific wins over base).
    pub fn effective_traces_endpoint(&self) -> Option<&str> {
        self.traces_endpoint.as_deref().or(self.endpoint.as_deref())
    }

    /// Resolve the effective traces protocol (signal-specific wins over base).
    pub fn effective_traces_protocol(&self) -> &OtelProtocol {
        self.traces_protocol.as_ref().unwrap_or(&self.protocol)
    }
}

/// Parse the `key=value,key2=value2` header format used by
/// `OTEL_EXPORTER_OTLP_HEADERS`.
pub(crate) fn parse_headers(s: &str) -> Vec<(String, String)> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?.trim();
            let value = parts.next()?.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Builder for [`OtelConfig`] (used by tests and programmatic callers, since the
/// environment cannot be mutated under `unsafe_code = "forbid"`).
#[derive(Debug, Default)]
pub struct OtelConfigBuilder {
    endpoint: Option<String>,
    traces_endpoint: Option<String>,
    protocol: Option<OtelProtocol>,
    traces_protocol: Option<OtelProtocol>,
    headers: Vec<(String, String)>,
    timeout: Option<Duration>,
    service_name: Option<String>,
    service_version: Option<String>,
}

impl OtelConfigBuilder {
    pub fn endpoint(mut self, e: impl Into<String>) -> Self {
        self.endpoint = Some(e.into());
        self
    }

    pub fn traces_endpoint(mut self, e: impl Into<String>) -> Self {
        self.traces_endpoint = Some(e.into());
        self
    }

    pub fn protocol(mut self, p: OtelProtocol) -> Self {
        self.protocol = Some(p);
        self
    }

    pub fn service_name(mut self, n: impl Into<String>) -> Self {
        self.service_name = Some(n.into());
        self
    }

    pub fn service_version(mut self, v: impl Into<String>) -> Self {
        self.service_version = Some(v.into());
        self
    }

    pub fn build(self) -> OtelConfig {
        OtelConfig {
            endpoint: self.endpoint,
            traces_endpoint: self.traces_endpoint,
            protocol: self.protocol.unwrap_or_default(),
            traces_protocol: self.traces_protocol,
            headers: self.headers,
            timeout: self.timeout.unwrap_or(Duration::from_secs(10)),
            service_name: self.service_name,
            service_version: self.service_version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_parses_spec_values_and_defaults() {
        assert_eq!("grpc".parse::<OtelProtocol>().unwrap(), OtelProtocol::Grpc);
        assert_eq!(
            "http/protobuf".parse::<OtelProtocol>().unwrap(),
            OtelProtocol::HttpProtobuf
        );
        assert_eq!(
            "http/json".parse::<OtelProtocol>().unwrap(),
            OtelProtocol::HttpJson
        );
        assert_eq!(
            "nonsense".parse::<OtelProtocol>().unwrap(),
            OtelProtocol::HttpProtobuf
        );
    }

    #[test]
    fn protocol_parse_is_case_insensitive_and_trims_surrounding_whitespace() {
        // The OTel spec values arrive from env vars that may carry casing/whitespace.
        assert_eq!("GRPC".parse::<OtelProtocol>().unwrap(), OtelProtocol::Grpc);
        assert_eq!(
            "  http/json  ".parse::<OtelProtocol>().unwrap(),
            OtelProtocol::HttpJson
        );
        assert_eq!(
            "Http/Protobuf".parse::<OtelProtocol>().unwrap(),
            OtelProtocol::HttpProtobuf
        );
        // Empty / whitespace-only fall back to the default rather than erroring.
        assert_eq!("".parse::<OtelProtocol>().unwrap(), OtelProtocol::default());
        assert_eq!(
            "   ".parse::<OtelProtocol>().unwrap(),
            OtelProtocol::default()
        );
    }

    #[test]
    fn headers_parse_edge_cases() {
        // Empty value is kept (a header with no value is still a header).
        assert_eq!(parse_headers("a="), vec![("a".to_string(), String::new())]);
        // A pair with no `=` separator is dropped, not treated as a keyless value.
        assert_eq!(parse_headers("nokey"), Vec::<(String, String)>::new());
        // Only the FIRST `=` splits; the value may itself contain `=` (e.g. base64).
        assert_eq!(
            parse_headers("auth=Bearer=abc=="),
            vec![("auth".to_string(), "Bearer=abc==".to_string())]
        );
        // Mixed: keyless value dropped, empty-key dropped, valid ones kept.
        assert_eq!(
            parse_headers("nokey, =v, k=val"),
            vec![("k".to_string(), "val".to_string())]
        );
    }

    #[test]
    fn effective_traces_protocol_prefers_signal_specific_over_base() {
        // The builder has no `traces_protocol` setter, so construct directly (all
        // fields are pub); the signal-specific protocol must win over the base one.
        let cfg = OtelConfig {
            protocol: OtelProtocol::HttpJson,
            traces_protocol: Some(OtelProtocol::Grpc),
            ..OtelConfig::default()
        };
        assert_eq!(cfg.effective_traces_protocol(), &OtelProtocol::Grpc);
    }

    #[test]
    fn effective_traces_endpoint_is_none_when_unconfigured() {
        let cfg = OtelConfig::default();
        assert_eq!(cfg.effective_traces_endpoint(), None);
        assert!(!cfg.is_configured());
    }

    #[test]
    fn headers_parse_key_value_pairs() {
        assert_eq!(parse_headers(""), Vec::<(String, String)>::new());
        assert_eq!(
            parse_headers("a=1, b = 2 ,=skip,c=3"),
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
                ("c".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn traces_endpoint_and_protocol_prefer_signal_specific() {
        let cfg = OtelConfig::builder()
            .endpoint("http://base:4318")
            .traces_endpoint("http://traces:4318/v1/traces")
            .build();
        assert!(cfg.is_configured());
        assert_eq!(
            cfg.effective_traces_endpoint(),
            Some("http://traces:4318/v1/traces")
        );

        let base_only = OtelConfig::builder().endpoint("http://base:4318").build();
        assert_eq!(
            base_only.effective_traces_endpoint(),
            Some("http://base:4318")
        );
        assert_eq!(
            base_only.effective_traces_protocol(),
            &OtelProtocol::HttpProtobuf
        );

        assert!(!OtelConfig::default().is_configured());
    }
}
