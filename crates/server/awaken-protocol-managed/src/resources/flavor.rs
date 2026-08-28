//! Endpoint-local beta/GA discrimination. The selector changes only the wire
//! projection; both variants call the same application service and repository.

use axum::http::HeaderMap;

use crate::common::headers::{ManagedCapability, has_capability};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedResourceApiFlavor {
    Beta,
    Ga,
}

/// Contract selected by a generated SDK's transport-only `beta=true` query when
/// its endpoint capability header is absent. Models keep their Beta projection;
/// Files/Skills SDKs at and after their GA change point retain the Beta namespace
/// and query while consuming the GA wire contract. Older Files/Skills SDKs still
/// send the capability header and therefore retain their historical projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BetaQueryPolicy {
    QuerySelectsBeta,
    QuerySelectsGa,
}

pub(crate) fn resource_api_flavor(
    raw_query: Option<&str>,
    headers: &HeaderMap,
    capability: ManagedCapability,
    query_policy: BetaQueryPolicy,
) -> Result<ManagedResourceApiFlavor, String> {
    let query_beta = raw_query
        .map(|query| form_urlencoded::parse(query.as_bytes()))
        .into_iter()
        .flatten()
        .filter(|(name, _)| name == "beta")
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    if query_beta.len() > 1 || query_beta.first().is_some_and(|value| value != "true") {
        return Err("the beta selector must be exactly `beta=true`".into());
    }
    let header_beta = has_capability(headers, capability);
    match (!query_beta.is_empty(), header_beta) {
        (true, true) | (false, true) => Ok(ManagedResourceApiFlavor::Beta),
        (true, false) => Ok(match query_policy {
            BetaQueryPolicy::QuerySelectsBeta => ManagedResourceApiFlavor::Beta,
            BetaQueryPolicy::QuerySelectsGa => ManagedResourceApiFlavor::Ga,
        }),
        (false, false) => Ok(ManagedResourceApiFlavor::Ga),
    }
}

/// Remove the generated SDK's transport-only `beta=true` selector before
/// deserializing the public parameter DTO. This keeps Rust SDK DTOs one-to-one
/// with the declarations while the route still validates the wire selector.
pub(super) fn without_beta_selector(raw_query: Option<&str>) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            raw_query
                .map(|raw| form_urlencoded::parse(raw.as_bytes()))
                .into_iter()
                .flatten()
                .filter(|(name, _)| name != "beta"),
        )
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn selector_has_one_unambiguous_beta_and_ga_path() {
        // Cause/effect graph: C1 beta query, C2 matching beta header, C3
        // malformed/duplicate selector, C4 endpoint query policy. Effects: E1
        // GA when neither selector is present; E2 beta for the historical
        // header-bearing form or the Models query-only form; E3 GA for the
        // Files/Skills change-point query-only form.
        // Decision table: R1 !C1&&!C2->E1; R2 C1&&C2->E2; R3 !C1&&C2->E2;
        // R4 C1&&!C2&&GA-policy->E3; R5 C1&&!C2&&Beta-policy->E2; R6 C3 rejects.
        // The selected flavor changes only projection and never chooses storage
        // or authorization.
        let mut headers = HeaderMap::new();
        assert_eq!(
            resource_api_flavor(
                None,
                &headers,
                ManagedCapability::Skills,
                BetaQueryPolicy::QuerySelectsGa,
            )
            .unwrap(),
            ManagedResourceApiFlavor::Ga,
            "R1"
        );
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static(ManagedCapability::Skills.beta()),
        );
        assert_eq!(
            resource_api_flavor(
                Some("beta=true"),
                &headers,
                ManagedCapability::Skills,
                BetaQueryPolicy::QuerySelectsGa,
            )
            .unwrap(),
            ManagedResourceApiFlavor::Beta,
            "R2"
        );
        assert_eq!(
            resource_api_flavor(
                None,
                &headers,
                ManagedCapability::Skills,
                BetaQueryPolicy::QuerySelectsGa,
            )
            .unwrap(),
            ManagedResourceApiFlavor::Beta,
            "R3"
        );
        assert_eq!(
            resource_api_flavor(
                Some("beta=true"),
                &HeaderMap::new(),
                ManagedCapability::Skills,
                BetaQueryPolicy::QuerySelectsGa,
            )
            .unwrap(),
            ManagedResourceApiFlavor::Ga,
            "R6"
        );
        assert!(
            resource_api_flavor(
                Some("beta=false"),
                &headers,
                ManagedCapability::Skills,
                BetaQueryPolicy::QuerySelectsGa,
            )
            .is_err(),
            "R4"
        );
        assert_eq!(
            without_beta_selector(Some("page=p1&beta=true&limit=5")),
            "page=p1&limit=5",
            "transport selector is not part of the public DTO"
        );
        assert_eq!(
            resource_api_flavor(
                Some("beta=true"),
                &HeaderMap::new(),
                ManagedCapability::ManagedAgents,
                BetaQueryPolicy::QuerySelectsBeta,
            )
            .unwrap(),
            ManagedResourceApiFlavor::Beta,
            "R5 historical official Models SDK query selects Beta"
        );
    }
}
