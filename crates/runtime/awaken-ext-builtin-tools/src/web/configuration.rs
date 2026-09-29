//! Typed Agent-owned Web execution policy and its runtime projection.

use awaken_runtime_contract::activation::InheritedToolsets;
use awaken_runtime_contract::agent_bindings::{ToolsetPolicy, ToolsetSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Extension-owned execution settings decoded from the neutral policy's one
/// opaque configuration value. The serde shape is the durable shape previously
/// stored by the neutral contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum WebExecutionConfiguration {
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
pub(super) struct EffectiveWebFetchConfiguration {
    pub domains: Vec<WebDomainFilter>,
    pub max_content_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct EffectiveWebSearchConfiguration {
    pub domains: Vec<WebDomainFilter>,
    pub user_location: Option<WebSearchUserLocation>,
}

impl From<WebFetchExecutionConfiguration> for EffectiveWebFetchConfiguration {
    fn from(value: WebFetchExecutionConfiguration) -> Self {
        Self {
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

pub(super) fn effective_fetch_configuration(
    own: Option<WebFetchExecutionConfiguration>,
    inherited: &InheritedToolsets,
) -> Result<Option<EffectiveWebFetchConfiguration>, String> {
    let mut present = own.is_some();
    let mut effective: EffectiveWebFetchConfiguration = own.map(Into::into).unwrap_or_default();
    for toolsets in inherited.sources() {
        if let Some(configuration) = web_fetch_execution_configuration(toolsets)? {
            present = true;
            effective.domains.extend(configuration.domains);
            effective.max_content_tokens = effective
                .max_content_tokens
                .or(configuration.max_content_tokens);
        }
    }
    Ok(present.then_some(effective))
}

pub(super) fn effective_search_configuration(
    own: Option<WebSearchExecutionConfiguration>,
    inherited: &InheritedToolsets,
) -> Result<Option<EffectiveWebSearchConfiguration>, String> {
    let mut present = own.is_some();
    let mut effective: EffectiveWebSearchConfiguration = own.map(Into::into).unwrap_or_default();
    for toolsets in inherited.sources() {
        if let Some(configuration) = web_search_execution_configuration(toolsets)? {
            present = true;
            effective.domains.extend(configuration.domains);
            effective.user_location = effective.user_location.or(configuration.user_location);
        }
    }
    Ok(present.then_some(effective))
}

pub(super) fn url_matches_filters(url: &url::Url, filters: &[WebDomainFilter]) -> bool {
    filters.iter().all(|filter| url_matches_filter(url, filter))
}

/// Each allowed region is a host subtree and a path subtree. A nonempty
/// intersection has a witness using a listed host and a listed path, possibly
/// from different lists. Blocks can remove a whole region, never only its root.
pub(super) fn domains_permit_any_url(filters: &[WebDomainFilter]) -> bool {
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

fn execution_configuration<'a>(toolsets: &'a [ToolsetPolicy], name: &str) -> Option<&'a Value> {
    toolsets
        .iter()
        .find(|policy| matches!(policy.source, ToolsetSource::Agent))
        .and_then(|policy| policy.configuration_for(name))
}

/// Read the normalized `web_fetch` settings from the executable toolset.
/// Authoring and runtime never maintain a parallel settings map.
pub fn web_fetch_execution_configuration(
    toolsets: &[ToolsetPolicy],
) -> Result<Option<WebFetchExecutionConfiguration>, String> {
    let Some(value) = execution_configuration(toolsets, "web_fetch") else {
        return Ok(None);
    };
    match serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid web_fetch execution configuration: {error}"))?
    {
        WebExecutionConfiguration::WebFetch(configuration) => Ok(Some(configuration)),
        WebExecutionConfiguration::WebSearch(_) => {
            Err("web_fetch policy carries web_search execution configuration".to_string())
        }
    }
}

/// Read the normalized `web_search` settings from the executable toolset. The
/// same value configures Native dispatch and ACP export.
pub fn web_search_execution_configuration(
    toolsets: &[ToolsetPolicy],
) -> Result<Option<WebSearchExecutionConfiguration>, String> {
    let Some(value) = execution_configuration(toolsets, "web_search") else {
        return Ok(None);
    };
    match serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid web_search execution configuration: {error}"))?
    {
        WebExecutionConfiguration::WebSearch(configuration) => Ok(Some(configuration)),
        WebExecutionConfiguration::WebFetch(_) => {
            Err("web_search policy carries web_fetch execution configuration".to_string())
        }
    }
}

pub(super) fn url_matches_filter(url: &url::Url, filter: &WebDomainFilter) -> bool {
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
mod tests {
    use super::*;
    use awaken_runtime_contract::agent_bindings::{ToolExecutionPolicy, ToolPolicyOverride};

    fn source(value: Value, name: &str) -> Vec<ToolsetPolicy> {
        vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy::default(),
            overrides: vec![ToolPolicyOverride::with_optional_configuration(
                name,
                ToolExecutionPolicy::default(),
                Some(value),
            )],
        }]
    }

    fn allow(domains: &[&str]) -> WebDomainFilter {
        WebDomainFilter::Allow(domains.iter().map(|value| (*value).to_string()).collect())
    }

    fn block(domains: &[&str]) -> WebDomainFilter {
        WebDomainFilter::Block(domains.iter().map(|value| (*value).to_string()).collect())
    }

    #[test]
    fn inherited_domain_decision_table_preserves_host_and_path_regions() {
        // Causes: C1 no lists, allow or block; C2 matching/nonmatching host;
        // C3 comparable/disjoint ancestor hosts; C4 comparable/disjoint paths;
        // C5 a block covers part/all of the allowed region. Effects: E1 URL
        // allowed/refused, E2 at least one reachable URL/no reachable URL.
        // Constraint: every list applies; no string intersection, suffix spoof,
        // path erasure or fallback to unrestricted on an empty intersection.
        // Rules below enumerate empty, single, mixed, nested and disjoint
        // regions, plus the host/path witness drawn from different sources.
        let rows = [
            ("D1", vec![], "https://other.example/", true, true),
            (
                "D2",
                vec![allow(&["example.com"])],
                "https://docs.example.com/",
                true,
                true,
            ),
            (
                "D3",
                vec![allow(&["example.com"])],
                "https://evil-example.com/",
                false,
                true,
            ),
            (
                "D4",
                vec![allow(&["example.com"]), allow(&["docs.example.com"])],
                "https://docs.example.com/",
                true,
                true,
            ),
            (
                "D5",
                vec![allow(&["example.com"]), allow(&["docs.example.com"])],
                "https://api.example.com/",
                false,
                true,
            ),
            (
                "D6",
                vec![allow(&["example.com"]), block(&["ads.example.com"])],
                "https://docs.example.com/",
                true,
                true,
            ),
            (
                "D7",
                vec![allow(&["example.com"]), block(&["ads.example.com"])],
                "https://img.ads.example.com/",
                false,
                true,
            ),
            (
                "D8",
                vec![block(&["ads.example.com"]), block(&["example.net"])],
                "https://example.net/",
                false,
                true,
            ),
            (
                "D9",
                vec![allow(&["example.com"]), allow(&["example.net"])],
                "https://example.com/",
                false,
                false,
            ),
            (
                "D10",
                vec![allow(&["example.com"]), block(&["example.com"])],
                "https://docs.example.com/",
                false,
                false,
            ),
            (
                "D11",
                vec![allow(&["example.com/blog"]), allow(&["docs.example.com"])],
                "https://docs.example.com/blog/post",
                true,
                true,
            ),
            (
                "D12",
                vec![allow(&["example.com/blog"]), allow(&["example.com/news"])],
                "https://example.com/blog/",
                false,
                false,
            ),
            (
                "D13",
                vec![allow(&["www.example.com"])],
                "https://example.com/",
                false,
                true,
            ),
            (
                "D14",
                vec![
                    allow(&["EXAMPLE.com/blog"]),
                    block(&["example.com/blog/ads"]),
                ],
                "https://docs.example.com/blog/post",
                true,
                true,
            ),
            (
                "D15",
                vec![allow(&[])],
                "https://example.com/",
                false,
                false,
            ),
        ];
        for (rule, filters, raw, reachable, any) in rows {
            assert_eq!(
                url_matches_filters(&url::Url::parse(raw).unwrap(), &filters),
                reachable,
                "{rule}/E1"
            );
            assert_eq!(domains_permit_any_url(&filters), any, "{rule}/E2");
        }
    }

    #[test]
    fn adding_a_domain_source_never_widens_reachability() {
        // Cause/effect property: for each admitted/refused URL and every
        // additional allow/block source, reachable(chain + source) implies
        // reachable(chain). Empty chains witness the positive unrestricted
        // case, so changing all->any or refusing an empty chain is observable.
        let filters = [
            allow(&["example.com"]),
            allow(&["docs.example.com"]),
            block(&["ads.example.com"]),
            block(&["example.com"]),
        ];
        for raw in [
            "https://docs.example.com/",
            "https://ads.example.com/",
            "https://example.net/",
        ] {
            let url = url::Url::parse(raw).unwrap();
            assert!(url_matches_filters(&url, &[]), "empty source chain");
            for prefix in [
                vec![],
                vec![filters[0].clone()],
                vec![filters[1].clone(), filters[2].clone()],
            ] {
                let before = url_matches_filters(&url, &prefix);
                for added in &filters {
                    let mut extended = prefix.clone();
                    extended.push(added.clone());
                    assert!(
                        !url_matches_filters(&url, &extended) || before,
                        "{raw}: extra source widened access"
                    );
                }
            }
        }
    }

    #[test]
    fn inherited_scalars_use_first_value_and_malformed_sources_fail_closed() {
        // Causes: C1 own scalar present/absent; C2 caller present/absent;
        // C3 coordinator present; C4 a source carries the wrong tool kind.
        // Effects: S1 own wins even when larger, S2 caller wins, S3 coordinator
        // supplies an omitted value, S4 location is one whole object (no merge),
        // S5 bad source rejects configuration. Domain sources always accumulate.
        let root = source(
            serde_json::json!({"type":"web_fetch","max_content_tokens":10,"domains":{"type":"block","domains":["ads.example.com"]}}),
            "web_fetch",
        );
        let caller = source(
            serde_json::json!({"type":"web_fetch","max_content_tokens":20}),
            "web_fetch",
        );
        let inherited = InheritedToolsets::with_caller(
            caller,
            Some(&InheritedToolsets::with_caller(root.clone(), None)),
        );
        let own = WebFetchExecutionConfiguration {
            domains: Some(allow(&["example.com"])),
            max_content_tokens: Some(100),
        };
        let effective = effective_fetch_configuration(Some(own), &inherited)
            .unwrap()
            .unwrap();
        assert_eq!(effective.max_content_tokens, Some(100), "S1");
        assert_eq!(effective.domains.len(), 2, "domain sources accumulate");
        assert_eq!(
            effective_fetch_configuration(None, &inherited)
                .unwrap()
                .unwrap()
                .max_content_tokens,
            Some(20),
            "S2"
        );
        assert_eq!(
            effective_fetch_configuration(None, &InheritedToolsets::with_caller(root, None))
                .unwrap()
                .unwrap()
                .max_content_tokens,
            Some(10),
            "S3"
        );
        let inherited = InheritedToolsets::with_caller(
            source(
                serde_json::json!({"type":"web_search","user_location":{"country":"US","city":"Boston"}}),
                "web_search",
            ),
            None,
        );
        let own = WebSearchExecutionConfiguration {
            domains: None,
            user_location: Some(WebSearchUserLocation {
                country: Some("SG".into()),
                city: None,
                region: None,
                timezone: None,
            }),
        };
        let effective = effective_search_configuration(Some(own.clone()), &inherited)
            .unwrap()
            .unwrap();
        assert_eq!(
            effective.user_location, own.user_location,
            "S4 no field merge"
        );
        assert_eq!(
            effective_search_configuration(None, &inherited)
                .unwrap()
                .unwrap()
                .user_location
                .unwrap()
                .city
                .as_deref(),
            Some("Boston"),
            "S2 search inheritance"
        );
        let wrong = InheritedToolsets::with_caller(
            source(serde_json::json!({"type":"web_search"}), "web_fetch"),
            None,
        );
        assert!(
            effective_fetch_configuration(None, &wrong).is_err(),
            "S5 wrong kind"
        );
        let unknown = InheritedToolsets::with_caller(
            source(
                serde_json::json!({"type":"web_fetch","unexpected":true}),
                "web_fetch",
            ),
            None,
        );
        assert!(
            effective_fetch_configuration(None, &unknown).is_err(),
            "S5 unknown member"
        );
        // S6: an explicit empty inherited chain carries no Web settings. It
        // must preserve the old `None` realization, including provider-native
        // route selection and unfiltered malformed search-result behavior.
        let empty = InheritedToolsets::with_caller(Vec::new(), None);
        assert!(
            effective_fetch_configuration(None, &empty)
                .unwrap()
                .is_none(),
            "S6 fetch"
        );
        assert!(
            effective_search_configuration(None, &empty)
                .unwrap()
                .is_none(),
            "S6 search"
        );
        let old = serde_json::json!({"type":"web_fetch","domains":{"type":"allow","domains":["example.com"]},"max_content_tokens":100});
        let decoded: WebExecutionConfiguration = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            old,
            "old singleton encoding stays exact"
        );
    }
}
