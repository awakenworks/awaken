// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: One Web-owned pure kernel parses, validates, composes and admits execution policy;
// @formal reason: the adjacent native tables cover typed and opaque inputs without effectful dependencies.
//! Pure Web execution policy: one parser, validator, composition and admission kernel.
use crate::{WebFetchUrlSources, WebUrlSourceKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// Stable field-level classification shared by compilation and execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebConfigurationCode {
    InvalidShape,
    WrongTool,
    ConflictingDomains,
    InvalidDomainCount,
    InvalidDomain,
    InvalidLocation,
    InvalidSources,
    UnavailableClientTool,
}

impl WebConfigurationCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidShape => "invalid_web_configuration_shape",
            Self::WrongTool => "web_configuration_owner_mismatch",
            Self::ConflictingDomains => "conflicting_web_domains",
            Self::InvalidDomainCount => "invalid_web_domain_count",
            Self::InvalidDomain => "invalid_web_domain",
            Self::InvalidLocation => "invalid_web_location",
            Self::InvalidSources => "invalid_web_url_sources",
            Self::UnavailableClientTool => "web_source_tool_unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebConfigurationIssue {
    pub path: String,
    pub code: WebConfigurationCode,
    pub message: String,
}

impl std::fmt::Display for WebConfigurationIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} [{}]: {}",
            self.path,
            self.code.as_str(),
            self.message
        )
    }
}
impl std::error::Error for WebConfigurationIssue {}

fn issue(
    path: impl Into<String>,
    code: WebConfigurationCode,
    message: impl Into<String>,
) -> WebConfigurationIssue {
    WebConfigurationIssue {
        path: path.into(),
        code,
        message: message.into(),
    }
}

/// Which facts the caller must supply when decoding Web configuration.
/// Publication checks the current client-tool roster. Frozen input keeps its
/// original roster decision, while syntax and intrinsic restrictions are rechecked.
/// Neither phase authorizes an individual Web request.
///
/// ```compile_fail
/// use awaken_web_policy_contract::WebConfigurationContext;
/// let _ = WebConfigurationContext::Publication {};
/// ```
#[derive(Debug, Clone, Copy)]
pub enum WebConfigurationContext<'a> {
    Publication { client_tools: &'a BTreeSet<&'a str> },
    Frozen,
}

impl<'a> WebConfigurationContext<'a> {
    fn client_tools(self) -> Option<&'a BTreeSet<&'a str>> {
        match self {
            Self::Publication { client_tools } => Some(client_tools),
            Self::Frozen => None,
        }
    }

    fn from_legacy(client_tools: Option<&'a BTreeSet<&'a str>>) -> Self {
        match client_tools {
            Some(client_tools) => Self::Publication { client_tools },
            None => Self::Frozen,
        }
    }
}

impl WebExecutionConfiguration {
    /// Decode Web settings at an explicit publication or frozen-input boundary.
    pub fn parse_in(
        tool: &str,
        value: &Value,
        context: WebConfigurationContext<'_>,
    ) -> Result<Self, WebConfigurationIssue> {
        let mut object = value.as_object().cloned().ok_or_else(|| {
            issue(
                "",
                WebConfigurationCode::InvalidShape,
                "Web configuration must be an object",
            )
        })?;
        if object
            .remove("type")
            .and_then(|value| value.as_str().map(str::to_owned))
            .as_deref()
            != Some(tool)
        {
            return Err(issue(
                "type",
                WebConfigurationCode::WrongTool,
                "execution configuration does not match its tool",
            ));
        }
        fn decode<T: serde::de::DeserializeOwned>(
            value: Value,
        ) -> Result<T, WebConfigurationIssue> {
            serde_path_to_error::deserialize(value).map_err(|error| {
                let path = error.path().to_string();
                issue(
                    if path == "." { String::new() } else { path },
                    WebConfigurationCode::InvalidShape,
                    error.inner().to_string(),
                )
            })
        }
        let payload = Value::Object(object);
        let configuration = match tool {
            "web_fetch" => Self::WebFetch(decode(payload)?),
            "web_search" => Self::WebSearch(decode(payload)?),
            _ => {
                return Err(issue(
                    "type",
                    WebConfigurationCode::WrongTool,
                    "unknown Web tool",
                ));
            }
        };
        configuration.validate_in(context)?;
        Ok(configuration)
    }

    pub fn validate_in(
        &self,
        context: WebConfigurationContext<'_>,
    ) -> Result<(), WebConfigurationIssue> {
        match self {
            Self::WebFetch(configuration) => configuration.validate(context.client_tools()),
            Self::WebSearch(configuration) => configuration.validate(),
        }
    }

    /// Parse, validate and normalize an opaque authored value atomically.
    pub fn compile_value_in(
        tool: &str,
        value: &Value,
        context: WebConfigurationContext<'_>,
    ) -> Result<Value, WebConfigurationIssue> {
        Self::parse_in(tool, value, context).map(|configuration| configuration.normalized_value())
    }

    /// Validate and normalize a typed wire projection in one fallible call.
    pub fn compile_in(
        &self,
        context: WebConfigurationContext<'_>,
    ) -> Result<Value, WebConfigurationIssue> {
        self.validate_in(context)?;
        Ok(self.normalized_value())
    }

    /// Compatibility entrypoint; new callers must state which boundary they use.
    #[deprecated(note = "use parse_in with an explicit WebConfigurationContext")]
    pub fn parse(
        tool: &str,
        value: &Value,
        clients: Option<&BTreeSet<&str>>,
    ) -> Result<Self, WebConfigurationIssue> {
        Self::parse_in(tool, value, WebConfigurationContext::from_legacy(clients))
    }

    #[deprecated(note = "use validate_in with an explicit WebConfigurationContext")]
    pub fn validate(&self, clients: Option<&BTreeSet<&str>>) -> Result<(), WebConfigurationIssue> {
        self.validate_in(WebConfigurationContext::from_legacy(clients))
    }

    #[deprecated(note = "use compile_value_in with an explicit WebConfigurationContext")]
    pub fn compile_value(
        tool: &str,
        value: &Value,
        clients: Option<&BTreeSet<&str>>,
    ) -> Result<Value, WebConfigurationIssue> {
        Self::compile_value_in(tool, value, WebConfigurationContext::from_legacy(clients))
    }

    #[deprecated(note = "use compile_in with an explicit WebConfigurationContext")]
    pub fn compile(
        &self,
        clients: Option<&BTreeSet<&str>>,
    ) -> Result<Value, WebConfigurationIssue> {
        self.compile_in(WebConfigurationContext::from_legacy(clients))
    }

    /// Preserve authored strings, list order, duplicate domains and scalar zero.
    fn normalized_value(&self) -> Value {
        serde_json::to_value(self).expect("closed Web configuration serializes")
    }
}

fn validate_domain_filter(
    filter: Option<&WebDomainFilter>,
    allow_path: bool,
) -> Result<(), WebConfigurationIssue> {
    let Some(WebDomainFilter::Allow(domains) | WebDomainFilter::Block(domains)) = filter else {
        return Ok(());
    };
    if domains.is_empty() || domains.len() > 64 {
        return Err(issue(
            "domains.domains",
            WebConfigurationCode::InvalidDomainCount,
            "must contain 1 to 64 domains",
        ));
    }
    for (index, value) in domains.iter().enumerate() {
        let (host, path) = value.split_once('/').unwrap_or((value, ""));
        let valid = !host.is_empty()
            && (allow_path || path.is_empty())
            && !value.contains("://")
            && !host.contains(':')
            && host.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        if !valid {
            return Err(issue(
                format!("domains.domains[{index}]"),
                WebConfigurationCode::InvalidDomain,
                "invalid domain",
            ));
        }
    }
    Ok(())
}

impl WebDomainFilter {
    /// Lower the wire's alternative list fields without silently choosing one.
    pub fn from_lists(
        allowed: Option<&[String]>,
        blocked: Option<&[String]>,
    ) -> Result<Option<Self>, WebConfigurationIssue> {
        match (allowed, blocked) {
            (Some(_), Some(_)) => Err(issue(
                "domains",
                WebConfigurationCode::ConflictingDomains,
                "allowed_domains and blocked_domains are mutually exclusive",
            )),
            (Some(domains), None) => Ok(Some(Self::Allow(domains.to_vec()))),
            (None, Some(domains)) => Ok(Some(Self::Block(domains.to_vec()))),
            (None, None) => Ok(None),
        }
    }
}

impl WebFetchExecutionConfiguration {
    pub fn validate(&self, clients: Option<&BTreeSet<&str>>) -> Result<(), WebConfigurationIssue> {
        validate_domain_filter(self.domains.as_ref(), false)?;
        if let Some(sources) = &self.url_sources {
            sources.validate_intrinsic().map_err(|message| {
                issue("url_sources", WebConfigurationCode::InvalidSources, message)
            })?;
            if let Some(clients) = clients {
                sources.validate(clients).map_err(|message| {
                    issue(
                        "url_sources.client_tool_results.tools",
                        WebConfigurationCode::UnavailableClientTool,
                        message,
                    )
                })?;
            }
        }
        Ok(())
    }
}

impl WebSearchExecutionConfiguration {
    pub fn validate(&self) -> Result<(), WebConfigurationIssue> {
        validate_domain_filter(self.domains.as_ref(), true)?;
        if let Some(location) = &self.user_location {
            if location.country.as_ref().is_some_and(|country| {
                country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_uppercase())
            }) {
                return Err(issue(
                    "user_location.country",
                    WebConfigurationCode::InvalidLocation,
                    "country must be two uppercase ASCII letters",
                ));
            }
            for (name, value) in [
                ("city", &location.city),
                ("region", &location.region),
                ("timezone", &location.timezone),
            ] {
                if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
                    return Err(issue(
                        format!("user_location.{name}"),
                        WebConfigurationCode::InvalidLocation,
                        "location value must not be blank",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl EffectiveWebFetchConfiguration {
    pub fn compose(
        own: Option<WebFetchExecutionConfiguration>,
        inherited: impl IntoIterator<Item = WebFetchExecutionConfiguration>,
    ) -> Result<Option<Self>, WebConfigurationIssue> {
        let mut present = own.is_some();
        let mut value: Self = own.map(Into::into).unwrap_or_default();
        for layer in inherited {
            present = true;
            value.url_sources.extend(layer.url_sources);
            value.domains.extend(layer.domains);
            value.max_content_tokens = value.max_content_tokens.or(layer.max_content_tokens);
        }
        value.validate()?;
        Ok(present.then_some(value))
    }

    pub fn validate(&self) -> Result<(), WebConfigurationIssue> {
        for filter in &self.domains {
            validate_domain_filter(Some(filter), false)?;
        }
        for sources in &self.url_sources {
            sources.validate_intrinsic().map_err(|message| {
                issue("url_sources", WebConfigurationCode::InvalidSources, message)
            })?;
        }
        Ok(())
    }
}

impl EffectiveWebSearchConfiguration {
    pub fn compose(
        own: Option<WebSearchExecutionConfiguration>,
        inherited: impl IntoIterator<Item = WebSearchExecutionConfiguration>,
    ) -> Result<Option<Self>, WebConfigurationIssue> {
        let mut present = own.is_some();
        let mut value: Self = own.map(Into::into).unwrap_or_default();
        for layer in inherited {
            // Validate before nearest-value precedence can discard a malformed layer.
            layer.validate()?;
            present = true;
            value.domains.extend(layer.domains);
            value.user_location = value.user_location.or(layer.user_location);
        }
        value.validate()?;
        Ok(present.then_some(value))
    }

    pub fn validate(&self) -> Result<(), WebConfigurationIssue> {
        for filter in &self.domains {
            validate_domain_filter(Some(filter), true)?;
        }
        WebSearchExecutionConfiguration {
            domains: None,
            user_location: self.user_location.clone(),
        }
        .validate()
    }
}

/// Explicit read-only provenance facts. Collection and evidence-version checks
/// belong to the Web transcript adapter; this value is not an authorization token.
#[derive(Debug, Clone, Copy)]
pub struct WebUrlSourceEvidence<'a> {
    pub kind: WebUrlSourceKind,
    pub name: &'a str,
    pub canonical_url: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebAdmissionFailure {
    InvalidConfiguration(WebConfigurationIssue),
    InvalidUrl(String),
    DomainRejected,
    SourceRejected,
}

impl std::fmt::Display for WebAdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(error) => error.fmt(f),
            Self::InvalidUrl(error) => write!(f, "url: {error}"),
            Self::DomainRejected => {
                f.write_str("url_not_allowed: web_fetch URL is outside the combined domain policy")
            }
            Self::SourceRejected => {
                f.write_str("url_not_allowed: URL was not shown by an allowed conversation source")
            }
        }
    }
}
impl std::error::Error for WebAdmissionFailure {}

/// Pure admission of an explicit URL and frozen conjunction of policies.
pub fn admit_fetch_url(
    raw: &str,
    domains: &[WebDomainFilter],
    sources: &[WebFetchUrlSources],
    facts: Option<&[WebUrlSourceEvidence<'_>]>,
) -> Result<url::Url, WebAdmissionFailure> {
    for domain in domains {
        validate_domain_filter(Some(domain), false)
            .map_err(WebAdmissionFailure::InvalidConfiguration)?;
    }
    for policy in sources {
        policy.validate_intrinsic().map_err(|message| {
            WebAdmissionFailure::InvalidConfiguration(issue(
                "url_sources",
                WebConfigurationCode::InvalidSources,
                message,
            ))
        })?;
    }
    let url =
        url::Url::parse(raw).map_err(|error| WebAdmissionFailure::InvalidUrl(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(WebAdmissionFailure::InvalidUrl(
            "an HTTP(S) URL with a host is required".into(),
        ));
    }
    if !url_matches_filters(&url, domains) {
        return Err(WebAdmissionFailure::DomainRejected);
    }
    if !sources.iter().all(|policy| {
        facts.is_some_and(|facts| {
            facts.iter().any(|fact| {
                fact.canonical_url == url.as_str() && policy.permits(fact.kind, fact.name)
            })
        })
    }) {
        return Err(WebAdmissionFailure::SourceRejected);
    }
    Ok(url)
}

#[cfg(test)]
mod kernel_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn opaque_and_typed_inputs_use_the_same_field_diagnostics() {
        // V1 valid fetch/search; V2 mismatched owner; V3 malformed/closed field;
        // V4 domain cardinality/syntax/path; V5 location; V6 source intrinsic
        // and publication roster. Canonicalization preserves duplicate domains,
        // scalar zero and strings; no rejected input can become a publication.
        let clients = BTreeSet::from(["lookup"]);
        let rows = [
            (
                "web_fetch",
                json!({"type":"web_fetch", "max_content_tokens":0, "domains":{"type":"allow","domains":["Example.COM", "Example.COM"]}}),
                None,
            ),
            (
                "web_search",
                json!({"type":"web_search","domains":{"type":"allow","domains":["example.com/docs"]},"user_location":{"country":"US","city":" New York "}}),
                None,
            ),
            (
                "web_fetch",
                json!({"type":"web_search"}),
                Some(WebConfigurationCode::WrongTool),
            ),
            (
                "web_fetch",
                json!({"type":"web_fetch","user_location":{}}),
                Some(WebConfigurationCode::InvalidShape),
            ),
            (
                "web_search",
                json!({"type":"web_search","max_content_tokens":0}),
                Some(WebConfigurationCode::InvalidShape),
            ),
            (
                "web_fetch",
                json!({"type":"web_fetch","domains":{"type":"allow","domains":[]}}),
                Some(WebConfigurationCode::InvalidDomainCount),
            ),
            (
                "web_fetch",
                json!({"type":"web_fetch","domains":{"type":"block","domains":["https://example.com"]}}),
                Some(WebConfigurationCode::InvalidDomain),
            ),
            (
                "web_fetch",
                json!({"type":"web_fetch","domains":{"type":"allow","domains":["example.com/docs"]}}),
                Some(WebConfigurationCode::InvalidDomain),
            ),
            (
                "web_search",
                json!({"type":"web_search","user_location":{"country":"us"}}),
                Some(WebConfigurationCode::InvalidLocation),
            ),
            (
                "web_search",
                json!({"type":"web_search","user_location":{"city":" \t"}}),
                Some(WebConfigurationCode::InvalidLocation),
            ),
            (
                "web_fetch",
                json!({"type":"web_fetch","url_sources":{"user_input":"none","client_tool_results":"none","server_tool_results":"none"}}),
                Some(WebConfigurationCode::InvalidSources),
            ),
            (
                "web_fetch",
                json!({"type":"web_fetch","url_sources":{"client_tool_results":{"type":"only","tools":[{"type":"tool_reference","name":"lookup"}]}}}),
                None,
            ),
            (
                "web_fetch",
                json!({"type":"web_fetch","url_sources":{"client_tool_results":{"type":"only","tools":[{"type":"tool_reference","name":"missing"}]}}}),
                Some(WebConfigurationCode::UnavailableClientTool),
            ),
        ];
        for (tool, authored, expected) in rows {
            match WebExecutionConfiguration::parse_in(
                tool,
                &authored,
                WebConfigurationContext::Publication {
                    client_tools: &clients,
                },
            ) {
                Ok(value) => {
                    assert_eq!(expected, None, "{authored}");
                    assert_eq!(
                        value.validate_in(WebConfigurationContext::Publication {
                            client_tools: &clients
                        }),
                        Ok(())
                    );
                    assert_eq!(
                        WebExecutionConfiguration::parse_in(
                            tool,
                            &value.normalized_value(),
                            WebConfigurationContext::Publication {
                                client_tools: &clients
                            }
                        )
                        .unwrap(),
                        value
                    );
                    if authored.get("max_content_tokens") == Some(&json!(0)) {
                        assert_eq!(value.normalized_value()["max_content_tokens"], 0);
                        assert_eq!(value.normalized_value()["domains"], authored["domains"]);
                    }
                }
                Err(error) => assert_eq!(Some(error.code), expected, "{authored}: {error}"),
            }
        }
        let typed = WebSearchExecutionConfiguration {
            domains: None,
            user_location: Some(WebSearchUserLocation {
                city: None,
                country: Some("us".into()),
                region: None,
                timezone: None,
            }),
        };
        let opaque = WebExecutionConfiguration::WebSearch(typed.clone()).normalized_value();
        assert_eq!(
            typed.validate().unwrap_err(),
            WebExecutionConfiguration::parse_in(
                "web_search",
                &opaque,
                WebConfigurationContext::Frozen
            )
            .unwrap_err()
        );
        assert_eq!(typed.validate().unwrap_err().path, "user_location.country");
    }

    #[test]
    fn composition_keeps_all_restrictions_and_nearest_scalar_without_erasing_invalid_layers() {
        // F1 every domain layer conjoins; F2 nearest max=0 remains zero; S1
        // nearest location wins only after every authored layer is validated.
        let own = WebFetchExecutionConfiguration {
            max_content_tokens: Some(0),
            domains: Some(WebDomainFilter::Allow(vec!["example.com".into()])),
            ..Default::default()
        };
        let parent = WebFetchExecutionConfiguration {
            max_content_tokens: Some(800),
            domains: Some(WebDomainFilter::Block(vec!["ads.example.com".into()])),
            ..Default::default()
        };
        let effective = EffectiveWebFetchConfiguration::compose(Some(own), [parent])
            .unwrap()
            .unwrap();
        assert_eq!(effective.max_content_tokens, Some(0));
        assert_eq!(effective.domains.len(), 2);
        assert!(
            admit_fetch_url("https://docs.example.com/", &effective.domains, &[], None).is_ok()
        );
        assert_eq!(
            admit_fetch_url("https://ads.example.com/", &effective.domains, &[], None),
            Err(WebAdmissionFailure::DomainRejected)
        );
        let own = WebSearchExecutionConfiguration {
            user_location: Some(WebSearchUserLocation {
                city: Some("Tokyo".into()),
                country: None,
                region: None,
                timezone: None,
            }),
            ..Default::default()
        };
        let bad_parent = WebSearchExecutionConfiguration {
            user_location: Some(WebSearchUserLocation {
                city: Some(" ".into()),
                country: None,
                region: None,
                timezone: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            EffectiveWebSearchConfiguration::compose(Some(own), [bad_parent])
                .unwrap_err()
                .code,
            WebConfigurationCode::InvalidLocation
        );
    }

    #[test]
    fn source_facts_are_explicit_and_each_policy_requires_its_own_allowed_witness() {
        // A1 no policy requires no source facts; A2 explicit source rule needs
        // exact URL and kind/name; A3 caller and callee may use distinct valid
        // witnesses for one URL, but a callee cannot widen a caller's sources.
        let url = "https://example.com/";
        let client: WebFetchUrlSources = serde_json::from_value(
            json!({"client_tool_results":"all","server_tool_results":"none","user_input":"none"}),
        )
        .unwrap();
        let server: WebFetchUrlSources = serde_json::from_value(
            json!({"client_tool_results":"none","server_tool_results":"all","user_input":"none"}),
        )
        .unwrap();
        let client_fact = WebUrlSourceEvidence {
            kind: WebUrlSourceKind::ClientTool,
            name: "web_search",
            canonical_url: url,
        };
        let server_fact = WebUrlSourceEvidence {
            kind: WebUrlSourceKind::ServerTool,
            name: "web_search",
            canonical_url: url,
        };
        assert!(admit_fetch_url(url, &[], &[], None).is_ok());
        assert_eq!(
            admit_fetch_url(url, &[], std::slice::from_ref(&client), None),
            Err(WebAdmissionFailure::SourceRejected)
        );
        assert!(
            admit_fetch_url(
                url,
                &[],
                std::slice::from_ref(&client),
                Some(&[client_fact])
            )
            .is_ok()
        );
        assert_eq!(
            admit_fetch_url(
                url,
                &[],
                std::slice::from_ref(&server),
                Some(&[client_fact])
            ),
            Err(WebAdmissionFailure::SourceRejected)
        );
        assert!(
            admit_fetch_url(
                url,
                &[],
                &[client.clone(), server.clone()],
                Some(&[client_fact, server_fact])
            )
            .is_ok()
        );
        assert_eq!(
            admit_fetch_url(url, &[], &[client, server], Some(&[server_fact])),
            Err(WebAdmissionFailure::SourceRejected)
        );
        assert_eq!(
            admit_fetch_url(
                "https://example.com/other",
                &[],
                &[WebFetchUrlSources::default()],
                Some(&[client_fact])
            ),
            Err(WebAdmissionFailure::SourceRejected)
        );
        assert!(matches!(
            admit_fetch_url("file:///tmp/a", &[], &[], None),
            Err(WebAdmissionFailure::InvalidUrl(_))
        ));
    }
}

/// Web-owned execution settings decoded from the neutral policy's one
/// opaque configuration value. The serde shape is the durable shape previously
/// stored by the neutral contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebExecutionConfiguration {
    WebFetch(WebFetchExecutionConfiguration),
    WebSearch(WebSearchExecutionConfiguration),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "domains",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WebDomainFilter {
    Allow(Vec<String>),
    Block(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebFetchExecutionConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_sources: Option<WebFetchUrlSources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<WebDomainFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchExecutionConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<WebDomainFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<WebSearchUserLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchUserLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// Derived execution values. The authored serde shapes above stay single-agent;
/// the Run's frozen caller sources are folded only at the extension boundary.
#[derive(Debug, Clone, Default)]
pub struct EffectiveWebFetchConfiguration {
    pub url_sources: Vec<WebFetchUrlSources>,
    pub domains: Vec<WebDomainFilter>,
    pub max_content_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct EffectiveWebSearchConfiguration {
    pub domains: Vec<WebDomainFilter>,
    pub user_location: Option<WebSearchUserLocation>,
}

impl From<WebFetchExecutionConfiguration> for EffectiveWebFetchConfiguration {
    fn from(value: WebFetchExecutionConfiguration) -> Self {
        Self {
            url_sources: value.url_sources.into_iter().collect(),
            domains: value.domains.into_iter().collect(),
            max_content_tokens: value.max_content_tokens,
        }
    }
}

impl From<WebSearchExecutionConfiguration> for EffectiveWebSearchConfiguration {
    fn from(value: WebSearchExecutionConfiguration) -> Self {
        Self {
            domains: value.domains.into_iter().collect(),
            user_location: value.user_location,
        }
    }
}

pub fn url_matches_filters(url: &url::Url, filters: &[WebDomainFilter]) -> bool {
    filters.iter().all(|filter| url_matches_filter(url, filter))
}

/// Each allowed region is a host subtree and a path subtree. A nonempty
/// intersection has a witness using a listed host and a listed path, possibly
/// from different lists. Blocks can remove a whole region, never only its root.
pub fn domains_permit_any_url(filters: &[WebDomainFilter]) -> bool {
    let allowed = filters
        .iter()
        .filter_map(|filter| match filter {
            WebDomainFilter::Allow(domains) => Some(domains),
            WebDomainFilter::Block(_) => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    if !filters
        .iter()
        .any(|filter| matches!(filter, WebDomainFilter::Allow(_)))
    {
        return true;
    }
    let mut hosts = std::collections::BTreeSet::new();
    let mut paths = std::collections::BTreeSet::new();
    for domain in allowed {
        let (host, path) = domain.split_once('/').unwrap_or((domain, ""));
        hosts.insert(host);
        paths.insert(path);
    }
    hosts.iter().any(|host| {
        paths.iter().any(|path| {
            url::Url::parse(&format!("https://{host}/{path}"))
                .is_ok_and(|url| url_matches_filters(&url, filters))
        })
    })
}

pub fn url_matches_filter(url: &url::Url, filter: &WebDomainFilter) -> bool {
    let matches = |configured: &str| {
        let (domain, path) = configured.split_once('/').unwrap_or((configured, ""));
        let host_matches = url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case(domain)
                || host
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{}", domain.to_ascii_lowercase()))
        });
        host_matches
            && (path.is_empty()
                || url.path() == format!("/{path}")
                || url.path().starts_with(&format!("/{path}/")))
    };
    match filter {
        WebDomainFilter::Allow(domains) => domains.iter().any(|domain| matches(domain)),
        WebDomainFilter::Block(domains) => !domains.iter().any(|domain| matches(domain)),
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn configuration_context_requires_exact_publication_roster_and_never_grants_fetch() {
        // C1 publication/frozen; C2 exact/absent/case-different roster; C3 typed/JSON.
        // Publication accepts only the exact reference. Frozen retains the original
        // roster decision, but neither decoded result grants missing URL evidence.
        let raw = json!({"type":"web_fetch", "url_sources": {
            "client_tool_results":{"type":"only","tools":[
                {"type":"tool_reference","name":"lookup"}
            ]}, "server_tool_results":"none", "user_input":"none"
        }});
        let typed: WebExecutionConfiguration = serde_json::from_value(raw.clone()).unwrap();
        for (roster, accepted) in [
            (BTreeSet::from(["lookup"]), true),
            (BTreeSet::new(), false),
            (BTreeSet::from(["Lookup"]), false),
        ] {
            let context = WebConfigurationContext::Publication {
                client_tools: &roster,
            };
            let opaque = WebExecutionConfiguration::compile_value_in("web_fetch", &raw, context);
            let projected = typed.compile_in(context);
            assert_eq!(opaque, projected);
            assert_eq!(opaque.is_ok(), accepted);
            if let Err(error) = opaque {
                assert_eq!(error.code, WebConfigurationCode::UnavailableClientTool);
                assert_eq!(error.path, "url_sources.client_tool_results.tools");
            }
        }
        let decoded =
            WebExecutionConfiguration::parse_in("web_fetch", &raw, WebConfigurationContext::Frozen)
                .unwrap();
        let WebExecutionConfiguration::WebFetch(fetch) = decoded else {
            panic!("exact tool kind")
        };
        let sources = [fetch.url_sources.unwrap()];
        assert_eq!(
            admit_fetch_url("https://example.com/", &[], &sources, None),
            Err(WebAdmissionFailure::SourceRejected)
        );
        let facts = [WebUrlSourceEvidence {
            kind: WebUrlSourceKind::ClientTool,
            name: "lookup",
            canonical_url: "https://example.com/",
        }];
        assert!(admit_fetch_url("https://example.com/", &[], &sources, Some(&facts)).is_ok());
    }

    #[test]
    fn configuration_context_rechecks_intrinsic_rules_for_frozen_and_typed_input() {
        // C1 stage; C2 wrong owner/unknown field/invalid domains/invalid source.
        // Intrinsic rejection and diagnostics are identical at both boundaries;
        // typed construction cannot bypass semantic validation or canonicalization.
        let clients = BTreeSet::from(["lookup"]);
        for raw in [
            json!({"type":"web_search"}),
            json!({"type":"web_fetch","extra":true}),
            json!({"type":"web_fetch","domains":{"type":"allow","domains":[]}}),
            json!({"type":"web_fetch","url_sources":{"user_input":"none","server_tool_results":"none","client_tool_results":"none"}}),
        ] {
            let authoring = WebExecutionConfiguration::parse_in(
                "web_fetch",
                &raw,
                WebConfigurationContext::Publication {
                    client_tools: &clients,
                },
            );
            let frozen = WebExecutionConfiguration::parse_in(
                "web_fetch",
                &raw,
                WebConfigurationContext::Frozen,
            );
            assert!(authoring.is_err());
            assert_eq!(authoring, frozen);
        }
        let invalid = WebExecutionConfiguration::WebFetch(WebFetchExecutionConfiguration {
            domains: Some(WebDomainFilter::Allow(Vec::new())),
            ..Default::default()
        });
        assert_eq!(
            invalid
                .compile_in(WebConfigurationContext::Frozen)
                .unwrap_err()
                .code,
            WebConfigurationCode::InvalidDomainCount
        );
        let raw = json!({"type":"web_fetch","domains":{"type":"allow","domains":["Example.COM","Example.COM"]},"max_content_tokens":0});
        assert_eq!(
            WebExecutionConfiguration::compile_value_in(
                "web_fetch",
                &raw,
                WebConfigurationContext::Frozen
            )
            .unwrap(),
            raw
        );
    }

    #[test]
    #[allow(deprecated)] // Intentional SDK compatibility test for the old Option API.
    fn legacy_configuration_context_keeps_its_original_results_and_diagnostics() {
        let clients = BTreeSet::from(["lookup"]);
        for raw in [
            json!({"type":"web_fetch","max_content_tokens":0}),
            json!({"type":"web_fetch","url_sources":{"client_tool_results":{"type":"only","tools":[{"type":"tool_reference","name":"missing"}]}}}),
            json!({"type":"web_fetch","domains":{"type":"allow","domains":[]}}),
        ] {
            for legacy in [Some(&clients), None] {
                let context = WebConfigurationContext::from_legacy(legacy);
                assert_eq!(
                    WebExecutionConfiguration::parse("web_fetch", &raw, legacy),
                    WebExecutionConfiguration::parse_in("web_fetch", &raw, context)
                );
                assert_eq!(
                    WebExecutionConfiguration::compile_value("web_fetch", &raw, legacy),
                    WebExecutionConfiguration::compile_value_in("web_fetch", &raw, context)
                );
            }
        }
    }
}
