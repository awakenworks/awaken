// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: One Web-owned pure kernel parses, validates, composes and admits execution policy;
// @formal reason: the adjacent native tables cover typed and opaque inputs without effectful dependencies.
//! Web adapter over the one pure SDK policy kernel.
use awaken_agent_contract::toolset::InheritedToolsets;
use awaken_agent_contract::{ToolsetPolicy, ToolsetSource};
pub(super) use awaken_web_policy_contract::WebExecutionConfiguration;
pub(super) use awaken_web_policy_contract::{
    EffectiveWebFetchConfiguration, EffectiveWebSearchConfiguration, domains_permit_any_url,
    url_matches_filters,
};
use awaken_web_policy_contract::{WebConfigurationContext, WebConfigurationIssue};
pub use awaken_web_policy_contract::{
    WebDomainFilter, WebFetchExecutionConfiguration, WebSearchExecutionConfiguration,
    WebSearchUserLocation,
};
use serde_json::Value;

pub(super) fn effective_fetch_configuration(
    own: Option<WebFetchExecutionConfiguration>,
    inherited: &InheritedToolsets,
) -> Result<Option<EffectiveWebFetchConfiguration>, WebConfigurationIssue> {
    let layers = inherited
        .sources()
        .iter()
        .map(|toolsets| decode_fetch_configuration(toolsets))
        .collect::<Result<Vec<_>, _>>()?;
    EffectiveWebFetchConfiguration::compose(own, layers.into_iter().flatten())
}
pub(super) fn effective_search_configuration(
    own: Option<WebSearchExecutionConfiguration>,
    inherited: &InheritedToolsets,
) -> Result<Option<EffectiveWebSearchConfiguration>, WebConfigurationIssue> {
    let layers = inherited
        .sources()
        .iter()
        .map(|toolsets| decode_search_configuration(toolsets))
        .collect::<Result<Vec<_>, _>>()?;
    EffectiveWebSearchConfiguration::compose(own, layers.into_iter().flatten())
}
fn execution_configuration<'a>(toolsets: &'a [ToolsetPolicy], name: &str) -> Option<&'a Value> {
    toolsets
        .iter()
        .find(|policy| matches!(policy.source, ToolsetSource::Agent))
        .and_then(|policy| policy.configuration_for(name))
}
pub fn web_fetch_execution_configuration(
    toolsets: &[ToolsetPolicy],
) -> Result<Option<WebFetchExecutionConfiguration>, String> {
    decode_fetch_configuration(toolsets).map_err(|error| error.to_string())
}

fn decode_fetch_configuration(
    toolsets: &[ToolsetPolicy],
) -> Result<Option<WebFetchExecutionConfiguration>, WebConfigurationIssue> {
    execution_configuration(toolsets, "web_fetch")
        .map(|value| {
            WebExecutionConfiguration::parse_in("web_fetch", value, WebConfigurationContext::Frozen)
                .map(|value| match value {
                    WebExecutionConfiguration::WebFetch(configuration) => configuration,
                    _ => unreachable!("parser checks owner"),
                })
        })
        .transpose()
}
pub fn web_search_execution_configuration(
    toolsets: &[ToolsetPolicy],
) -> Result<Option<WebSearchExecutionConfiguration>, String> {
    decode_search_configuration(toolsets).map_err(|error| error.to_string())
}

fn decode_search_configuration(
    toolsets: &[ToolsetPolicy],
) -> Result<Option<WebSearchExecutionConfiguration>, WebConfigurationIssue> {
    execution_configuration(toolsets, "web_search")
        .map(|value| {
            WebExecutionConfiguration::parse_in(
                "web_search",
                value,
                WebConfigurationContext::Frozen,
            )
            .map(|value| match value {
                WebExecutionConfiguration::WebSearch(configuration) => configuration,
                _ => unreachable!("parser checks owner"),
            })
        })
        .transpose()
}
#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::{ToolExecutionPolicy, ToolPolicyOverride};

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
    fn frozen_decoding_preserves_structured_diagnostics_until_compatibility_boundary() {
        // C1 absent/valid/wrong owner; C2 fetch/search; C3 invalid nested field.
        // Absence is not an error. Malformed input retains code and field path;
        // the existing SDK String adapter is exactly the final Display projection.
        use awaken_web_policy_contract::WebConfigurationCode;
        assert!(decode_fetch_configuration(&[]).unwrap().is_none());
        assert!(decode_search_configuration(&[]).unwrap().is_none());
        for (name, raw, code, path) in [
            (
                "web_fetch",
                serde_json::json!({"type":"web_search"}),
                WebConfigurationCode::WrongTool,
                "type",
            ),
            (
                "web_fetch",
                serde_json::json!({"type":"web_fetch","domains":{"type":"allow","domains":[]}}),
                WebConfigurationCode::InvalidDomainCount,
                "domains.domains",
            ),
            (
                "web_search",
                serde_json::json!({"type":"web_fetch"}),
                WebConfigurationCode::WrongTool,
                "type",
            ),
            (
                "web_search",
                serde_json::json!({"type":"web_search","user_location":{"country":"sg"}}),
                WebConfigurationCode::InvalidLocation,
                "user_location.country",
            ),
        ] {
            let toolsets = source(raw, name);
            let (issue, legacy) = if name == "web_fetch" {
                (
                    decode_fetch_configuration(&toolsets).unwrap_err(),
                    web_fetch_execution_configuration(&toolsets).unwrap_err(),
                )
            } else {
                (
                    decode_search_configuration(&toolsets).unwrap_err(),
                    web_search_execution_configuration(&toolsets).unwrap_err(),
                )
            };
            assert_eq!(issue.code, code, "{name}");
            assert_eq!(issue.path, path, "{name}");
            assert!(!issue.message.is_empty());
            assert_eq!(legacy, issue.to_string(), "compatibility projection");
        }
        assert!(
            decode_fetch_configuration(&source(
                serde_json::json!({"type":"web_fetch"}),
                "web_fetch"
            ))
            .unwrap()
            .is_some()
        );
        assert!(
            decode_search_configuration(&source(
                serde_json::json!({"type":"web_search"}),
                "web_search"
            ))
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn inherited_composition_keeps_typed_failures_for_own_and_shadowed_sources() {
        // C1 malformed own typed value; C2 malformed inherited value even when
        // a nearer scalar would shadow it. Both paths retain the kernel issue;
        // no fallback to None or successful nearest-value selection is allowed.
        use awaken_web_policy_contract::WebConfigurationCode;
        let empty = InheritedToolsets::with_caller(Vec::new(), None);
        let issue = effective_fetch_configuration(
            Some(WebFetchExecutionConfiguration {
                domains: Some(allow(&[])),
                ..Default::default()
            }),
            &empty,
        )
        .unwrap_err();
        assert_eq!(issue.code, WebConfigurationCode::InvalidDomainCount);
        assert_eq!(issue.path, "domains.domains");
        let inherited = InheritedToolsets::with_caller(
            source(
                serde_json::json!({"type":"web_search","user_location":{"country":"bad"}}),
                "web_search",
            ),
            None,
        );
        let own = Some(WebSearchExecutionConfiguration {
            user_location: Some(WebSearchUserLocation {
                country: Some("SG".into()),
                city: None,
                region: None,
                timezone: None,
            }),
            ..Default::default()
        });
        let issue = effective_search_configuration(own.clone(), &inherited).unwrap_err();
        assert_eq!(issue.code, WebConfigurationCode::InvalidLocation);
        assert_eq!(issue.path, "user_location.country");
        assert!(
            effective_search_configuration(own, &empty)
                .unwrap()
                .is_some(),
            "valid positive control"
        );
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
            url_sources: None,
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
