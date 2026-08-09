//! Canonical projection of an A2A Agent Card security declaration.
//!
//! Publication and launch consume this one projection so supported anonymous /
//! single-header requirements and their fingerprint cannot drift.

use awaken_protocol_a2a::{AgentCard, ApiKeyLocation, SecurityScheme};
use awaken_runtime_contract::CredentialUsage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct A2aSecurityProfile {
    pub(crate) fingerprint: String,
    pub(crate) anonymous: bool,
    pub(crate) accepted_headers: Vec<CredentialUsage>,
}

pub(crate) fn project_agent_card_security(card: &AgentCard) -> Result<A2aSecurityProfile, String> {
    let fingerprint =
        awaken_runtime_contract::content_fingerprint(&(&card.security_schemes, &card.security))
            .map(|fingerprint| format!("sha256:{fingerprint}"))
            .map_err(|error| format!("fingerprint A2A Agent Card security: {error}"))?;
    if card.security.is_empty() {
        return Ok(A2aSecurityProfile {
            fingerprint,
            anonymous: true,
            accepted_headers: Vec::new(),
        });
    }
    let anonymous = card
        .security
        .iter()
        .any(std::collections::BTreeMap::is_empty);
    let mut accepted_headers = card
        .security
        .iter()
        .filter_map(|requirement| {
            if requirement.len() != 1 {
                return None;
            }
            let (name, _scopes) = requirement.iter().next()?;
            card.security_schemes.get(name).and_then(header_usage)
        })
        .collect::<Vec<_>>();
    accepted_headers.sort_by(|left, right| {
        serde_json::to_string(left)
            .expect("CredentialUsage serializes")
            .cmp(&serde_json::to_string(right).expect("CredentialUsage serializes"))
    });
    accepted_headers.dedup();
    if !anonymous && accepted_headers.is_empty() {
        return Err(
            "A2A Agent Card requires authentication, but no supported single HTTP-header scheme is available"
                .into(),
        );
    }
    Ok(A2aSecurityProfile {
        fingerprint,
        anonymous,
        accepted_headers,
    })
}

fn header_usage(scheme: &SecurityScheme) -> Option<CredentialUsage> {
    match scheme {
        SecurityScheme::ApiKey {
            name,
            location: ApiKeyLocation::Header,
            ..
        } => Some(CredentialUsage::HttpHeader {
            name: name.clone(),
            scheme: None,
        }),
        SecurityScheme::Http { scheme, .. } => Some(CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some(scheme.clone()),
        }),
        SecurityScheme::OAuth2 { .. } | SecurityScheme::OpenIdConnect { .. } => {
            Some(CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            })
        }
        SecurityScheme::ApiKey { .. } | SecurityScheme::MutualTls { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(security: serde_json::Value) -> AgentCard {
        let mut card = awaken_protocol_a2a::agent_card("remote");
        card.url = "https://agent.example".into();
        let security = security.as_object().expect("security fixture");
        card.security_schemes = serde_json::from_value(
            security
                .get("schemes")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        )
        .expect("security schemes");
        card.security = serde_json::from_value(
            security
                .get("requirements")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        )
        .expect("security requirements");
        card
    }

    #[test]
    fn card_security_projects_anonymous_and_supported_header_requirements() {
        // Cause graph: C1 the card declares requirements; C2 one OR branch is
        // anonymous; C3 a non-anonymous branch contains exactly one supported
        // header scheme. Effects: E1 preserve anonymous access; E2 expose the
        // exact header usage; E3 fingerprint the complete declaration.
        //
        // Decision table:
        // | Rule | C1 | C2 | C3 | Effect |
        // | S1   | N  | -  | -  | anonymous |
        // | S2   | Y  | Y  | Y  | E1+E2+E3 |
        // | S3   | Y  | N  | N  | fail closed |
        let anonymous = project_agent_card_security(&card(serde_json::json!({}))).unwrap();
        assert!(anonymous.anonymous);
        assert!(anonymous.accepted_headers.is_empty());

        let optional = project_agent_card_security(&card(serde_json::json!({
            "schemes": {
                "api": {"type": "apiKey", "name": "x-api-key", "in": "header"}
            },
            "requirements": [{}, {"api": []}]
        })))
        .unwrap();
        assert!(optional.anonymous);
        assert_eq!(
            optional.accepted_headers,
            vec![CredentialUsage::HttpHeader {
                name: "x-api-key".into(),
                scheme: None,
            }]
        );
        assert!(optional.fingerprint.starts_with("sha256:"));

        assert!(
            project_agent_card_security(&card(serde_json::json!({
                "schemes": {
                    "query": {"type": "apiKey", "name": "key", "in": "query"}
                },
                "requirements": [{"query": []}]
            })))
            .is_err()
        );
    }
}
