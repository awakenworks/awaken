//! Endpoint-local beta/GA discrimination. The selector changes only the wire
//! projection; both variants call the same application service and repository.

use axum::http::HeaderMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedResourceApiFlavor {
    Beta,
    Ga,
}

/// Whether the generated SDK's transport-only `beta=true` query is sufficient
/// to select Beta. Historical Models SDKs use the query alone; endpoint-specific
/// Files/Skills previews require their matching beta header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BetaQueryPolicy {
    QuerySelectsBeta,
    RequireHeader,
}

pub(crate) fn resource_api_flavor(
    raw_query: Option<&str>,
    headers: &HeaderMap,
    beta_name: &str,
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
    let header_beta = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim() == beta_name);
    match (!query_beta.is_empty(), header_beta) {
        (true, true) | (false, true) => Ok(ManagedResourceApiFlavor::Beta),
        (true, false) if query_policy == BetaQueryPolicy::QuerySelectsBeta => {
            Ok(ManagedResourceApiFlavor::Beta)
        }
        (true, false) => Err(format!(
            "`beta=true` requires `anthropic-beta: {beta_name}`"
        )),
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
        // GA when neither selector is present; E2 beta for the strict official
        // pair, header-only form, or the historical Models query-only form; E3
        // reject query-only selection on strict Files/Skills endpoints.
        // Decision table: R1 !C1&&!C2->E1; R2 C1&&C2->E2; R3 !C1&&C2->E2;
        // R4 (C1&&!C2&&!C4)||C3->E3; R5 C1&&!C2&&C4->E2. The selected flavor
        // changes only projection and never chooses storage or authorization.
        let mut headers = HeaderMap::new();
        assert_eq!(
            resource_api_flavor(
                None,
                &headers,
                "skills-beta",
                BetaQueryPolicy::RequireHeader,
            )
            .unwrap(),
            ManagedResourceApiFlavor::Ga,
            "R1"
        );
        headers.insert("anthropic-beta", HeaderValue::from_static("skills-beta"));
        assert_eq!(
            resource_api_flavor(
                Some("beta=true"),
                &headers,
                "skills-beta",
                BetaQueryPolicy::RequireHeader,
            )
            .unwrap(),
            ManagedResourceApiFlavor::Beta,
            "R2"
        );
        assert_eq!(
            resource_api_flavor(
                None,
                &headers,
                "skills-beta",
                BetaQueryPolicy::RequireHeader,
            )
            .unwrap(),
            ManagedResourceApiFlavor::Beta,
            "R3"
        );
        assert!(
            resource_api_flavor(
                Some("beta=true"),
                &HeaderMap::new(),
                "skills-beta",
                BetaQueryPolicy::RequireHeader,
            )
            .is_err(),
            "R4"
        );
        assert!(
            resource_api_flavor(
                Some("beta=false"),
                &headers,
                "skills-beta",
                BetaQueryPolicy::RequireHeader,
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
                "managed-beta",
                BetaQueryPolicy::QuerySelectsBeta,
            )
            .unwrap(),
            ManagedResourceApiFlavor::Beta,
            "R5 historical official Models SDK query selects Beta"
        );
    }
}
