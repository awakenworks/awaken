//! Endpoint-local transport-surface discrimination. The selector changes only
//! the wire projection; every surface calls the same application service and
//! repository.

use axum::http::HeaderMap;

use crate::common::headers::{ManagedCapability, has_capability};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedResourceApiSurface {
    /// A pre-GA SDK's explicit endpoint capability selects its legacy Beta DTO.
    CapabilityBeta,
    /// A post-GA SDK still called through `client.beta.*` and `beta=true`.
    QueryBeta,
    /// The top-level GA namespace, with no Beta selector or capability.
    Ga,
}

pub(crate) fn resource_api_surface(
    raw_query: Option<&str>,
    headers: &HeaderMap,
    capability: ManagedCapability,
) -> Result<ManagedResourceApiSurface, String> {
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
        (true, true) | (false, true) => Ok(ManagedResourceApiSurface::CapabilityBeta),
        (true, false) => Ok(ManagedResourceApiSurface::QueryBeta),
        (false, false) => Ok(ManagedResourceApiSurface::Ga),
    }
}

/// Remove the generated SDK's transport-only `beta=true` selector before
/// deserializing the public parameter DTO. This keeps Rust SDK DTOs one-to-one
/// with the declarations while the route still validates the wire selector.
pub(crate) fn without_beta_selector(raw_query: Option<&str>) -> String {
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
    fn selector_preserves_the_three_generated_sdk_surfaces() {
        // Cause/effect graph: C1 beta query, C2 matching beta header, C3
        // malformed/duplicate selector. Effects: E1 GA when neither selector
        // is present; E2 CapabilityBeta for the historical header-bearing
        // form; E3 QueryBeta for the post-GA Beta namespace. Family adapters
        // decide whether QueryBeta shares the GA DTO or owns a hybrid DTO; the
        // selector never chooses storage, authorization, or behavior by SDK
        // version.
        // Decision table: R1 !C1&&!C2->E1; R2 C2->E2; R3 C1&&!C2->E3;
        // R4 C3 rejects.
        let mut headers = HeaderMap::new();
        assert_eq!(
            resource_api_surface(None, &headers, ManagedCapability::Skills).unwrap(),
            ManagedResourceApiSurface::Ga,
            "R1"
        );
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static(ManagedCapability::Skills.beta()),
        );
        assert_eq!(
            resource_api_surface(Some("beta=true"), &headers, ManagedCapability::Skills).unwrap(),
            ManagedResourceApiSurface::CapabilityBeta,
            "R2"
        );
        assert_eq!(
            resource_api_surface(None, &headers, ManagedCapability::Skills).unwrap(),
            ManagedResourceApiSurface::CapabilityBeta,
            "R2 header-only"
        );
        assert_eq!(
            resource_api_surface(
                Some("beta=true"),
                &HeaderMap::new(),
                ManagedCapability::Skills,
            )
            .unwrap(),
            ManagedResourceApiSurface::QueryBeta,
            "R3"
        );
        assert!(
            resource_api_surface(Some("beta=false"), &headers, ManagedCapability::Skills,).is_err(),
            "R4"
        );
        assert_eq!(
            without_beta_selector(Some("page=p1&beta=true&limit=5")),
            "page=p1&limit=5",
            "transport selector is not part of the public DTO"
        );
    }
}
