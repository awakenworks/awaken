//! Last-mile HTTP projection for already-admitted credential material.

use awaken_runtime_contract::{CredentialAccess, CredentialMaterial, CredentialMaterialError};

/// Convert already-admitted material into the narrow HTTP credential shared by
/// MCP, A2A, and builtin Web adapters. Provider code receives only the final
/// header shape; it cannot reinterpret Vault material or choose another usage.
pub fn materialized_http_credential(
    access: &CredentialAccess,
    material: &CredentialMaterial,
) -> Result<awaken_credential::Credential, CredentialMaterialError> {
    material.validate_usage(&access.usage)?;
    let awaken_runtime_contract::CredentialUsage::HttpHeader { name, scheme } = &access.usage
    else {
        return Err(CredentialMaterialError::MaterialKindMismatch);
    };
    let secret = material.single_secret()?.expose_secret();
    if name.eq_ignore_ascii_case("authorization")
        && scheme
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("bearer"))
    {
        return Ok(awaken_credential::Credential::Bearer(secret.to_owned()));
    }
    let value = scheme
        .as_ref()
        .map_or_else(|| secret.to_owned(), |scheme| format!("{scheme} {secret}"));
    Ok(awaken_credential::Credential::Header {
        name: name.clone(),
        value,
    })
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::RedactedString;
    use awaken_runtime_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterial, CredentialMaterialError,
        CredentialMaterialSource, CredentialRef, CredentialUsage, ModelExposurePolicy,
        PlaintextBoundary, PlaintextHolder,
    };

    use super::materialized_http_credential;

    fn http_access(usage: CredentialUsage) -> CredentialAccess {
        let holder = PlaintextHolder::new(
            PlaintextBoundary::Worker,
            awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        );
        CredentialAccess::new(
            CredentialRef {
                id: "credential-a".into(),
                revision: 1,
            },
            CredentialMaterialSource::ControlPlaneReference,
            usage,
            CredentialExecutionPolicy::exact(holder, ModelExposurePolicy::Forbidden),
        )
    }

    #[test]
    fn materialized_http_credential_has_one_usage_owned_wire_shape() {
        // Cause/effect graph: C1 Authorization+Bearer; C2 arbitrary header
        // without scheme; C3 arbitrary header with scheme; C4 non-header
        // usage. E1 canonical Bearer; E2 exact header/value; E3 prefixed exact
        // header/value; E4 fail closed before a provider sees material.
        // Decision table H1=C1=>E1, H2=C2=>E2, H3=C3=>E3, H4=C4=>E4.
        let material = CredentialMaterial::secret(RedactedString::new("secret-value"));
        let cases = [
            (
                "H1",
                CredentialUsage::HttpHeader {
                    name: "Authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                awaken_credential::Credential::Bearer("secret-value".into()),
            ),
            (
                "H2",
                CredentialUsage::HttpHeader {
                    name: "X-Subscription-Token".into(),
                    scheme: None,
                },
                awaken_credential::Credential::Header {
                    name: "X-Subscription-Token".into(),
                    value: "secret-value".into(),
                },
            ),
            (
                "H3",
                CredentialUsage::HttpHeader {
                    name: "X-Api-Key".into(),
                    scheme: Some("Token".into()),
                },
                awaken_credential::Credential::Header {
                    name: "X-Api-Key".into(),
                    value: "Token secret-value".into(),
                },
            ),
        ];
        for (rule, usage, expected) in cases {
            assert_eq!(
                materialized_http_credential(&http_access(usage), &material).unwrap(),
                expected,
                "{rule}"
            );
        }
        assert_eq!(
            materialized_http_credential(
                &http_access(CredentialUsage::EnvironmentVariable {
                    name: "SECRET".into()
                }),
                &material,
            ),
            Err(CredentialMaterialError::MaterialKindMismatch),
            "H4"
        );
    }
}
