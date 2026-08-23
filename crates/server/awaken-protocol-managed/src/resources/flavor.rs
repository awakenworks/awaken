//! Endpoint-local beta/GA discrimination. The selector changes only the wire
//! projection; both variants call the same application service and repository.

use axum::http::HeaderMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManagedResourceApiFlavor {
    Beta,
    Ga,
}

pub(super) fn resource_api_flavor(
    raw_query: Option<&str>,
    headers: &HeaderMap,
    beta_name: &str,
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
    match (query_beta.first().is_some(), header_beta) {
        (true, true) | (false, true) => Ok(ManagedResourceApiFlavor::Beta),
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
        // malformed/duplicate selector. Effects: E1 GA when neither is present;
        // E2 beta when the official pair (or legacy header-only form) is present;
        // E3 reject a query that could otherwise silently hit the GA projection.
        // Decision table: R1 !C1&&!C2->E1; R2 C1&&C2->E2; R3 !C1&&C2->E2;
        // R4 C1&&!C2 or C3->E3. The selected flavor never chooses storage.
        let mut headers = HeaderMap::new();
        assert_eq!(
            resource_api_flavor(None, &headers, "skills-beta").unwrap(),
            ManagedResourceApiFlavor::Ga,
            "R1"
        );
        headers.insert("anthropic-beta", HeaderValue::from_static("skills-beta"));
        assert_eq!(
            resource_api_flavor(Some("beta=true"), &headers, "skills-beta").unwrap(),
            ManagedResourceApiFlavor::Beta,
            "R2"
        );
        assert_eq!(
            resource_api_flavor(None, &headers, "skills-beta").unwrap(),
            ManagedResourceApiFlavor::Beta,
            "R3"
        );
        assert!(
            resource_api_flavor(Some("beta=true"), &HeaderMap::new(), "skills-beta").is_err(),
            "R4"
        );
        assert!(
            resource_api_flavor(Some("beta=false"), &headers, "skills-beta").is_err(),
            "R4"
        );
        assert_eq!(
            without_beta_selector(Some("page=p1&beta=true&limit=5")),
            "page=p1&limit=5",
            "transport selector is not part of the public DTO"
        );
    }
}
