use awaken_credential_contract::{CredentialDescriptor, CredentialMaterial};
use awaken_credential_vault::{CredentialError, CredentialKind};

use super::{CredentialMaterialInput, EnterCredentialRequest, validate_hosted_credential_identity};

impl EnterCredentialRequest {
    /// Build the isolated compatibility body for legacy provider-id callers.
    /// Native hosted products must use [`Self::described_hosted_vault`] so
    /// provider, material shape, target, and usage have one typed authority.
    pub fn compat_hosted_vault(
        workspace_id: String,
        provider_id: String,
        idempotency_key: String,
        material: CredentialMaterial,
    ) -> Result<Self, CredentialError> {
        validate_hosted_credential_identity(&workspace_id, &provider_id, &idempotency_key)?;
        let (secret, material) = hosted_material_input(material)?;
        Ok(Self {
            workspace_id,
            idempotency_key: Some(idempotency_key),
            kind: CredentialKind::Vault,
            provider_id: Some(provider_id),
            descriptor: None,
            env_key: None,
            secret,
            material,
            oauth_helper: None,
        })
    }

    /// Build the canonical write-only hosted body from one validated descriptor
    /// and its matching typed material. The provider comes only from the
    /// descriptor; the mutually exclusive legacy `provider_id` is never set.
    pub fn described_hosted_vault(
        workspace_id: String,
        idempotency_key: String,
        descriptor: CredentialDescriptor,
        material: CredentialMaterial,
    ) -> Result<Self, CredentialError> {
        descriptor
            .validate()
            .map_err(|error| CredentialError::InvalidSource(error.to_string()))?;
        descriptor
            .validate_material(&material)
            .map_err(|error| CredentialError::InvalidSource(error.to_string()))?;
        for contract in &descriptor.targets {
            material
                .validate_usage(&contract.usage)
                .map_err(|error| CredentialError::InvalidSource(error.to_string()))?;
        }
        validate_hosted_credential_identity(
            &workspace_id,
            &descriptor.provider.0,
            &idempotency_key,
        )?;
        let (secret, material) = hosted_material_input(material)?;
        Ok(Self {
            workspace_id,
            idempotency_key: Some(idempotency_key),
            kind: CredentialKind::Vault,
            provider_id: None,
            descriptor: Some(descriptor),
            env_key: None,
            secret,
            material,
            oauth_helper: None,
        })
    }

    #[must_use]
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    #[must_use]
    pub fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }
}

fn hosted_material_input(
    material: CredentialMaterial,
) -> Result<(Option<String>, Option<CredentialMaterialInput>), CredentialError> {
    match material {
        CredentialMaterial::Secret(secret) => Ok((Some(secret.expose_secret().to_owned()), None)),
        CredentialMaterial::Structured(material) => Ok((
            None,
            Some(CredentialMaterialInput {
                type_id: material.type_id,
                fields: material
                    .fields
                    .into_iter()
                    .map(|(name, value)| (name, value.expose_secret().to_owned()))
                    .collect(),
            }),
        )),
        CredentialMaterial::OAuth(_) => Err(CredentialError::InvalidSource(
            "hosted Vault credentials do not accept OAuth material".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::RedactedString;
    use awaken_credential_contract::{
        CredentialMaterialDescriptor, CredentialPurpose, CredentialTarget,
        CredentialTargetContract, CredentialUsage, HTTP_BASIC_MATERIAL_TYPE, http_basic_material,
        repository_transport_audience,
    };

    use super::*;

    /// Hosted request cause/effect graph: C1 one validated descriptor owns the
    /// provider, material shape, target, and usage; C2 material matches that
    /// descriptor; C3 a legacy caller supplies only provider id; C4 material is
    /// OAuth. Effects: E1 one described request with no provider_id; E2 shape
    /// drift is rejected before serialization; E3 the explicitly named compat
    /// request remains descriptor-free; E4 OAuth cannot become stored material.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | Effect |
    /// |---|---|---|---|---|---|
    /// | H1 | T | T | F | F | E1 |
    /// | H2 | T | F | F | F | E2 |
    /// | H3 | F | - | T | F | E3 |
    /// | H4 | T | - | F | T | E4 |
    #[test]
    fn hosted_constructor_keeps_described_and_compat_authorities_disjoint() {
        let target = CredentialTarget::new(
            CredentialPurpose::RepositoryTransport,
            repository_transport_audience("https://github.com/awaken/example.git")
                .expect("H1 target"),
        );
        let descriptor = CredentialDescriptor::new(
            "github",
            CredentialMaterialDescriptor::structured(
                HTTP_BASIC_MATERIAL_TYPE,
                ["password", "username"],
            ),
            [CredentialTargetContract::new(
                target,
                CredentialUsage::HttpBasicAuth,
            )],
        );
        let material = CredentialMaterial::Structured(http_basic_material(
            RedactedString::new("x-access-token"),
            RedactedString::new("token"),
        ));
        let described = EnterCredentialRequest::described_hosted_vault(
            "workspace-a".into(),
            "operation-1".into(),
            descriptor.clone(),
            material,
        )
        .expect("H1/E1");
        assert!(described.provider_id.is_none(), "H1/E1");
        assert_eq!(described.descriptor.as_ref(), Some(&descriptor), "H1/E1");

        assert!(
            EnterCredentialRequest::described_hosted_vault(
                "workspace-a".into(),
                "operation-2".into(),
                descriptor.clone(),
                CredentialMaterial::secret(RedactedString::new("wrong-shape")),
            )
            .is_err(),
            "H2/E2"
        );

        let compat = EnterCredentialRequest::compat_hosted_vault(
            "workspace-a".into(),
            "legacy-provider".into(),
            "operation-3".into(),
            CredentialMaterial::secret(RedactedString::new("legacy")),
        )
        .expect("H3/E3");
        assert_eq!(compat.provider_id(), Some("legacy-provider"), "H3/E3");
        assert!(compat.descriptor.is_none(), "H3/E3");

        assert!(
            EnterCredentialRequest::described_hosted_vault(
                "workspace-a".into(),
                "operation-4".into(),
                descriptor,
                CredentialMaterial::OAuth(awaken_credential_contract::OAuthCredentialMaterial {
                    access_token: RedactedString::new("access"),
                    refresh_token: RedactedString::new("refresh"),
                    expires_at_unix_ms: None,
                    account_id: None,
                    account_plan: None,
                }),
            )
            .is_err(),
            "H4/E4"
        );
    }
}
