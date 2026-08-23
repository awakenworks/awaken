use awaken_credential_contract::CredentialMaterial;
use awaken_credential_vault::{CredentialError, CredentialKind};

use super::{CredentialMaterialInput, EnterCredentialRequest, validate_hosted_credential_identity};

impl EnterCredentialRequest {
    /// Build the canonical write-only body used by hosted governance to enter
    /// one idempotent Vault credential. OAuth remains helper-owned and cannot be
    /// downgraded to stored material through this boundary.
    pub fn hosted_vault(
        workspace_id: String,
        provider_id: String,
        idempotency_key: String,
        material: CredentialMaterial,
    ) -> Result<Self, CredentialError> {
        validate_hosted_credential_identity(&workspace_id, &provider_id, &idempotency_key)?;
        let (secret, material) = match material {
            CredentialMaterial::Secret(secret) => (Some(secret.expose_secret().to_owned()), None),
            CredentialMaterial::Structured(material) => (
                None,
                Some(CredentialMaterialInput {
                    type_id: material.type_id,
                    fields: material
                        .fields
                        .into_iter()
                        .map(|(name, value)| (name, value.expose_secret().to_owned()))
                        .collect(),
                }),
            ),
            CredentialMaterial::OAuth(_) => {
                return Err(CredentialError::InvalidSource(
                    "hosted Vault credentials do not accept OAuth material".into(),
                ));
            }
        };
        Ok(Self {
            workspace_id,
            idempotency_key: Some(idempotency_key),
            kind: CredentialKind::Vault,
            provider_id: Some(provider_id),
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
