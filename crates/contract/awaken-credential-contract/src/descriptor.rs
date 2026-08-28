use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{CredentialMaterial, CredentialUsage};

/// The only scalar material descriptor. A scalar secret carries no embedded
/// type identifier, so arbitrary typed scalar ids would be unverifiable
/// metadata. More specific types must use a versioned structured material.
pub const OPAQUE_SECRET_MATERIAL_TYPE: &str = "awaken.secret/v1";

fn valid_metadata_ref(value: &str) -> bool {
    !value.trim().is_empty() && value.trim() == value && !value.chars().any(char::is_control)
}

/// Provider namespace recorded as secret-free credential metadata. This names
/// the upstream provider (`github`), never a product resource kind such as
/// `github_repository`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct CredentialProviderRef(pub String);

/// Namespaced, versioned material shape identifier.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct CredentialMaterialTypeId(pub String);

/// Opaque provider subject such as one account or installation. This is
/// metadata for binding/probing, not an IAM principal.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct CredentialSubjectRef(pub String);

/// Exact upstream audience authorized for a credential purpose.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct CredentialAudienceRef(pub String);

impl From<String> for CredentialAudienceRef {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for CredentialAudienceRef {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Normalize an HTTPS repository remote to the provider-neutral Git transport
/// service audience shared by source issuers and execution consumers. Repository
/// identity stays in the existing material binding; credentials are reusable
/// across repositories only on the exact normalized origin.
pub fn repository_transport_audience(
    remote_url: &str,
) -> Result<CredentialAudienceRef, CredentialDescriptorError> {
    let remote = url::Url::parse(remote_url)
        .map_err(|_| CredentialDescriptorError::InvalidRepositoryAudience)?;
    if remote.scheme() != "https"
        || remote.host_str().is_none()
        || !remote.username().is_empty()
        || remote.password().is_some()
    {
        return Err(CredentialDescriptorError::InvalidRepositoryAudience);
    }
    let origin = remote.origin().ascii_serialization();
    if origin == "null" {
        return Err(CredentialDescriptorError::InvalidRepositoryAudience);
    }
    Ok(CredentialAudienceRef(format!("{origin}/git")))
}

/// Provider-owned permission/scope label exposed only as secret-free metadata.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct CredentialPermissionRef(pub String);

/// Why a target is allowed to consume the credential. Purpose is orthogonal to
/// provider identity and material shape.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialPurpose {
    ProviderAdapter,
    RepositoryTransport,
    McpAuthorization,
    RemoteAgentAuthorization,
    /// One registered Host Web provider identified by its immutable provider id.
    WebProviderAuthorization,
    HttpEffect,
    SignatureVerification,
    Extension,
}

impl CredentialPurpose {
    /// Validate the closed usage shapes supported by each target owner.
    pub fn validate_usage(&self, usage: &CredentialUsage) -> Result<(), CredentialDescriptorError> {
        match (self, usage) {
            (Self::ProviderAdapter, CredentialUsage::ProviderAdapter)
            | (Self::ProviderAdapter, CredentialUsage::EnvironmentVariable { .. })
            | (Self::ProviderAdapter, CredentialUsage::File { .. })
            | (Self::RepositoryTransport, CredentialUsage::HttpBasicAuth)
            | (Self::McpAuthorization, CredentialUsage::HttpHeader { .. })
            | (Self::RemoteAgentAuthorization, CredentialUsage::HttpHeader { .. })
            | (Self::WebProviderAuthorization, CredentialUsage::HttpHeader { .. })
            | (Self::WebProviderAuthorization, CredentialUsage::HttpBasicAuth)
            | (Self::WebProviderAuthorization, CredentialUsage::QueryParameter { .. })
            | (Self::WebProviderAuthorization, CredentialUsage::ClientCertificate)
            | (Self::WebProviderAuthorization, CredentialUsage::EnvironmentVariable { .. })
            | (Self::WebProviderAuthorization, CredentialUsage::File { .. })
            | (Self::HttpEffect, CredentialUsage::HttpEffect { .. })
            | (Self::SignatureVerification, CredentialUsage::SignatureVerification { .. })
            | (Self::Extension, CredentialUsage::Extension { .. }) => Ok(()),
            _ => Err(CredentialDescriptorError::InvalidTargetUsage),
        }
    }
}

/// Exact purpose/audience identity frozen into executable access. Usage remains
/// the existing independent field on `CredentialAccess`, so executable payloads
/// cannot serialize two copies of the same usage truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialTarget {
    pub purpose: CredentialPurpose,
    pub audience: CredentialAudienceRef,
}

impl CredentialTarget {
    #[must_use]
    pub fn new(purpose: CredentialPurpose, audience: impl Into<CredentialAudienceRef>) -> Self {
        Self {
            purpose,
            audience: audience.into(),
        }
    }

    /// Validate the purpose/usage pair without deriving or duplicating usage.
    /// Consumer-owned target constructors normalize audiences; descriptor
    /// publication independently rechecks Repository canonicality so a wire
    /// caller cannot bypass that constructor.
    pub fn validate_usage(&self, usage: &CredentialUsage) -> Result<(), CredentialDescriptorError> {
        self.purpose.validate_usage(usage)
    }
}

/// Validate the single target/usage authority carried by executable access.
/// Raw local injection forms retain their holder/binding authority; every
/// network or provider effect must name its exact consumer target.
pub fn validate_credential_target_usage(
    target: Option<&CredentialTarget>,
    usage: &CredentialUsage,
) -> Result<(), CredentialDescriptorError> {
    match target {
        Some(target) => target.validate_usage(usage),
        None if matches!(
            usage,
            CredentialUsage::ProviderAdapter
                | CredentialUsage::HttpHeader { .. }
                | CredentialUsage::HttpBasicAuth
                | CredentialUsage::HttpEffect { .. }
                | CredentialUsage::SignatureVerification { .. }
                | CredentialUsage::Extension { .. }
        ) =>
        {
            Err(CredentialDescriptorError::MissingTarget)
        }
        None => Ok(()),
    }
}

/// One source-declared authorization contract. The target identity and its
/// allowed material usage are indivisible here, while executable access carries
/// that target identity plus its existing single `usage` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialTargetContract {
    pub target: CredentialTarget,
    pub usage: CredentialUsage,
}

impl CredentialTargetContract {
    #[must_use]
    pub fn new(target: CredentialTarget, usage: CredentialUsage) -> Self {
        Self { target, usage }
    }
}

/// Secret-free description of the material document sealed by the Vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialMaterialDescriptor {
    Secret {
        type_id: CredentialMaterialTypeId,
    },
    Structured {
        type_id: CredentialMaterialTypeId,
        fields: BTreeSet<String>,
    },
}

impl CredentialMaterialDescriptor {
    #[must_use]
    pub fn secret(type_id: impl Into<String>) -> Self {
        Self::Secret {
            type_id: CredentialMaterialTypeId(type_id.into()),
        }
    }

    #[must_use]
    pub fn structured<I, S>(type_id: impl Into<String>, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Structured {
            type_id: CredentialMaterialTypeId(type_id.into()),
            fields: fields.into_iter().map(Into::into).collect(),
        }
    }

    fn type_id(&self) -> &CredentialMaterialTypeId {
        match self {
            Self::Secret { type_id } | Self::Structured { type_id, .. } => type_id,
        }
    }
}

/// One normalized, secret-free description stored on the canonical source row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialDescriptor {
    pub provider: CredentialProviderRef,
    pub material: CredentialMaterialDescriptor,
    /// Exact authorized target/usage contracts. Separate sets are deliberately
    /// forbidden because their Cartesian product would invent undeclared uses.
    pub targets: Vec<CredentialTargetContract>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub permissions: BTreeSet<CredentialPermissionRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<CredentialSubjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
}

impl CredentialDescriptor {
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        material: CredentialMaterialDescriptor,
        targets: impl IntoIterator<Item = CredentialTargetContract>,
    ) -> Self {
        Self {
            provider: CredentialProviderRef(provider.into()),
            material,
            targets: targets.into_iter().collect(),
            permissions: BTreeSet::new(),
            subject: None,
            expires_at_unix_ms: None,
        }
    }

    #[must_use]
    pub fn with_permissions<I, S>(mut self, permissions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.permissions = permissions
            .into_iter()
            .map(|value| CredentialPermissionRef(value.into()))
            .collect();
        self
    }

    #[must_use]
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(CredentialSubjectRef(subject.into()));
        self
    }

    #[must_use]
    pub fn with_expiry(mut self, expires_at_unix_ms: u64) -> Self {
        self.expires_at_unix_ms = Some(expires_at_unix_ms);
        self
    }

    pub fn validate(&self) -> Result<(), CredentialDescriptorError> {
        if !valid_metadata_ref(&self.provider.0)
            || !valid_metadata_ref(&self.material.type_id().0)
            || self.targets.is_empty()
            || self
                .targets
                .iter()
                .any(|contract| !valid_metadata_ref(&contract.target.audience.0))
            || self
                .permissions
                .iter()
                .any(|permission| !valid_metadata_ref(&permission.0))
            || self
                .subject
                .as_ref()
                .is_some_and(|subject| !valid_metadata_ref(&subject.0))
        {
            return Err(CredentialDescriptorError::InvalidMetadata);
        }
        for (index, contract) in self.targets.iter().enumerate() {
            contract
                .usage
                .validate()
                .map_err(|_| CredentialDescriptorError::InvalidTargetUsage)?;
            contract.target.validate_usage(&contract.usage)?;
            if contract.target.purpose == CredentialPurpose::RepositoryTransport
                && repository_transport_audience(&contract.target.audience.0)?
                    != contract.target.audience
            {
                return Err(CredentialDescriptorError::InvalidRepositoryAudience);
            }
            if self.targets[index + 1..]
                .iter()
                .any(|candidate| candidate.target.eq(&contract.target))
            {
                return Err(CredentialDescriptorError::DuplicateTarget);
            }
        }
        if let CredentialMaterialDescriptor::Structured { fields, .. } = &self.material
            && (fields.is_empty() || fields.iter().any(|field| !valid_metadata_ref(field)))
        {
            return Err(CredentialDescriptorError::InvalidMaterialShape);
        }
        if let CredentialMaterialDescriptor::Secret { type_id } = &self.material
            && type_id.0 != OPAQUE_SECRET_MATERIAL_TYPE
        {
            return Err(CredentialDescriptorError::InvalidMaterialShape);
        }
        if self.expires_at_unix_ms == Some(0) {
            return Err(CredentialDescriptorError::InvalidExpiry);
        }
        Ok(())
    }

    pub fn admit(
        &self,
        target: &CredentialTarget,
        usage: &CredentialUsage,
    ) -> Result<(), CredentialDescriptorError> {
        self.validate()?;
        if !valid_metadata_ref(&target.audience.0)
            || !self
                .targets
                .iter()
                .any(|contract| &contract.target == target && &contract.usage == usage)
        {
            return Err(CredentialDescriptorError::TargetMismatch);
        }
        Ok(())
    }

    pub fn validate_expiry(&self, now_unix_ms: u64) -> Result<(), CredentialDescriptorError> {
        if self
            .expires_at_unix_ms
            .is_some_and(|expires_at| now_unix_ms >= expires_at)
        {
            return Err(CredentialDescriptorError::Expired);
        }
        Ok(())
    }

    /// Prove that opened plaintext exactly matches this source descriptor before
    /// an adapter can consume it.
    pub fn validate_material(
        &self,
        material: &CredentialMaterial,
    ) -> Result<(), CredentialDescriptorError> {
        self.validate()?;
        let matches = match (material, &self.material) {
            (CredentialMaterial::Secret(_), CredentialMaterialDescriptor::Secret { .. }) => true,
            (
                CredentialMaterial::Structured(material),
                CredentialMaterialDescriptor::Structured { type_id, fields },
            ) => material.type_id == type_id.0 && material.fields.keys().eq(fields.iter()),
            _ => false,
        };
        matches
            .then_some(())
            .ok_or(CredentialDescriptorError::MaterialMismatch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CredentialDescriptorError {
    #[error("credential purpose has no exact target compiler")]
    UnsupportedPurpose,
    #[error("credential descriptor metadata is invalid")]
    InvalidMetadata,
    #[error("credential descriptor material shape is invalid")]
    InvalidMaterialShape,
    #[error("credential descriptor expiry is invalid")]
    InvalidExpiry,
    #[error("credential target is not declared by the source descriptor")]
    TargetMismatch,
    #[error("credential descriptor declares an invalid target usage")]
    InvalidTargetUsage,
    #[error("credential usage requires an exact target")]
    MissingTarget,
    #[error("credential descriptor declares the same purpose and audience more than once")]
    DuplicateTarget,
    #[error("credential material does not match the source descriptor")]
    MaterialMismatch,
    #[error("credential material is expired")]
    Expired,
    #[error("repository credential audience requires an HTTPS origin without userinfo")]
    InvalidRepositoryAudience,
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use awaken_agent_contract::{RedactedString, StructuredCredentialMaterial};

    use super::*;
    use crate::{HTTP_BASIC_MATERIAL_TYPE, http_basic_material};

    /// Cause/effect graph: C1 descriptor syntax is valid; C2 provider and exact
    /// target set are nonempty; C3 exact target purpose/audience is declared;
    /// C4 material type and complete field set equal the descriptor; C5 static
    /// expiry is still in the future; C6 access usage equals the target-declared
    /// usage. Effects: E1 exact target/material admitted;
    /// E2 malformed metadata rejected; E3 undeclared target rejected; E4 material
    /// mismatch rejected; E5 expired material rejected before plaintext use;
    /// E6 a purpose/audience match cannot authorize another usage.
    ///
    /// | Rule | C1+C2 | C3 | C4 | C5 | Effect |
    /// |---|---|---|---|---|---|
    /// | G1 | T | T | T | T | E1 |
    /// | G2 | F | - | - | - | E2 |
    /// | G3 | T | F | - | T | E3 |
    /// | G4 | T | T | F | T | E4 |
    /// | G5 | T | T | T | F | E5 |
    /// | G6 | T | T but usage differs | T | T | E6 |
    #[test]
    fn admission_binds_target_shape_and_expiry_exactly() {
        let target = CredentialTarget::new(
            CredentialPurpose::RepositoryTransport,
            repository_transport_audience("https://github.com/awaken/example.git").unwrap(),
        );
        let usage = crate::CredentialUsage::HttpBasicAuth;
        let descriptor = CredentialDescriptor::new(
            "github",
            CredentialMaterialDescriptor::structured(
                HTTP_BASIC_MATERIAL_TYPE,
                ["password", "username"],
            ),
            [CredentialTargetContract::new(target.clone(), usage.clone())],
        )
        .with_expiry(2_000);
        let material = CredentialMaterial::Structured(http_basic_material(
            RedactedString::new("x-access-token"),
            RedactedString::new("token"),
        ));

        assert!(descriptor.validate().is_ok(), "G1 metadata");
        assert!(descriptor.admit(&target, &usage).is_ok(), "G1 target");
        assert!(descriptor.validate_material(&material).is_ok(), "G1 shape");
        assert!(descriptor.validate_expiry(1_999).is_ok(), "G1 expiry");

        let malformed = CredentialDescriptor::new(
            " ",
            CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE),
            [],
        );
        assert!(malformed.validate().is_err(), "G2/E2");
        let unverifiable_scalar_type = CredentialDescriptor::new(
            "github",
            CredentialMaterialDescriptor::secret("vendor.typed-scalar/v1"),
            [CredentialTargetContract::new(target.clone(), usage.clone())],
        );
        assert!(
            matches!(
                unverifiable_scalar_type.validate(),
                Err(CredentialDescriptorError::InvalidMaterialShape)
            ),
            "G2/E2 scalar bytes cannot prove an arbitrary type id"
        );
        let undeclared = CredentialTarget::new(
            CredentialPurpose::McpAuthorization,
            "https://other.example.test/mcp",
        );
        assert!(descriptor.admit(&undeclared, &usage).is_err(), "G3/E3");
        assert!(
            descriptor
                .validate_material(&CredentialMaterial::Structured(
                    StructuredCredentialMaterial {
                        type_id: HTTP_BASIC_MATERIAL_TYPE.into(),
                        fields: BTreeMap::from([
                            ("password".into(), RedactedString::new("token"),)
                        ]),
                    },
                ))
                .is_err(),
            "G4/E4"
        );
        assert!(descriptor.validate_expiry(2_000).is_err(), "G5/E5");
        assert!(
            descriptor
                .admit(
                    &target,
                    &crate::CredentialUsage::HttpHeader {
                        name: "authorization".into(),
                        scheme: Some("Bearer".into()),
                    },
                )
                .is_err(),
            "G6/E6"
        );
    }

    /// Cause/effect graph: C1 remote is HTTPS with host and no userinfo; C2 path
    /// differs on the same origin; C3 origin differs; C4 a descriptor supplied
    /// over the wire already contains the canonical origin `/git` audience.
    /// Effects: E1 same-origin repositories normalize to one reusable `/git`
    /// audience; E2 unsafe syntax is rejected; E3 another origin cannot satisfy
    /// the declared target; E4 invalid or noncanonical descriptor audiences are
    /// rejected before publication.
    ///
    /// | Rule | C1 | same origin | C4 | Effect |
    /// |---|---|---|---|---|
    /// | H1 | T | T | T | E1 |
    /// | H2 | F | - | - | E2 |
    /// | H3 | T | F | T | E3 |
    /// | H4 | T/F | - | F | E4 |
    #[test]
    fn repository_audience_is_normalized_and_host_exact() {
        let first =
            repository_transport_audience("https://GitHub.com/awaken/first.git?ignored=true")
                .expect("H1 first repository");
        let second = repository_transport_audience("https://github.com/awaken/second.git")
            .expect("H1 second repository");
        assert_eq!(first.0, "https://github.com/git", "H1/E1");
        assert_eq!(first, second, "H1/E1");
        assert!(
            repository_transport_audience("http://github.com/awaken/first.git").is_err(),
            "H2/E2 insecure transport"
        );
        assert!(
            repository_transport_audience("https://token@github.com/awaken/first.git").is_err(),
            "H2/E2 userinfo"
        );

        let usage = crate::CredentialUsage::HttpBasicAuth;
        let declared = CredentialTarget::new(CredentialPurpose::RepositoryTransport, first);
        let descriptor = CredentialDescriptor::new(
            "github",
            CredentialMaterialDescriptor::structured(
                HTTP_BASIC_MATERIAL_TYPE,
                ["password", "username"],
            ),
            [CredentialTargetContract::new(declared, usage.clone())],
        );
        assert!(descriptor.validate().is_ok(), "H1/E1 canonical descriptor");
        let other_host = CredentialTarget::new(
            CredentialPurpose::RepositoryTransport,
            repository_transport_audience("https://github.example/awaken/first.git")
                .expect("H3 other HTTPS origin"),
        );
        assert!(descriptor.admit(&other_host, &usage).is_err(), "H3/E3");

        for (rule, audience) in [
            ("H4-noncanonical", "https://github.com/awaken/first.git"),
            ("H4-invalid", "github.com/git"),
        ] {
            let invalid = CredentialDescriptor::new(
                "github",
                CredentialMaterialDescriptor::structured(
                    HTTP_BASIC_MATERIAL_TYPE,
                    ["password", "username"],
                ),
                [CredentialTargetContract::new(
                    CredentialTarget::new(CredentialPurpose::RepositoryTransport, audience),
                    usage.clone(),
                )],
            );
            assert_eq!(
                invalid.validate(),
                Err(CredentialDescriptorError::InvalidRepositoryAudience),
                "{rule}/E4"
            );
        }
    }

    /// Purpose/usage cause-effect graph: C1 purpose has a production exact-
    /// target compiler; C2 usage is that purpose's sole supported shape.
    /// Effects: E1 the pair is admitted; E2 a cross-purpose usage is rejected;
    /// E3 every network/provider usage fails without its target.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// |---|---|---|---|
    /// | P1 | T | T | E1 |
    /// | P2 | T | F | E2 |
    /// | P3 | T | target absent | E3 |
    #[test]
    fn credential_purpose_accepts_only_its_exact_supported_usage() {
        let effect = CredentialUsage::HttpEffect {
            fields: BTreeMap::from([(
                "token".into(),
                BTreeSet::from([crate::HttpEffectPlacement::Header {
                    name: "authorization".into(),
                }]),
            )]),
        };
        let extension = CredentialUsage::Extension {
            consumer_id: "acme.ssh-agent/v1".into(),
            material_type: "acme.ssh-key/v1".into(),
            public_config: serde_json::json!({}),
        };
        let signature = CredentialUsage::SignatureVerification {
            fields: BTreeMap::from([(
                "webhook_secret".into(),
                BTreeSet::from([crate::SignatureVerificationAlgorithm::HmacSha256]),
            )]),
        };

        assert_eq!(
            CredentialPurpose::RepositoryTransport.validate_usage(&CredentialUsage::HttpBasicAuth),
            Ok(()),
            "P1/E1 Repository"
        );
        assert_eq!(
            CredentialPurpose::HttpEffect.validate_usage(&effect),
            Ok(()),
            "P1/E1 HTTP effect"
        );
        assert_eq!(
            CredentialPurpose::Extension.validate_usage(&extension),
            Ok(()),
            "P1/E1 extension"
        );
        assert_eq!(
            CredentialPurpose::SignatureVerification.validate_usage(&signature),
            Ok(()),
            "P1/E1 signature verification"
        );
        assert_eq!(
            CredentialPurpose::RepositoryTransport.validate_usage(&effect),
            Err(CredentialDescriptorError::InvalidTargetUsage),
            "P2/E2"
        );
        assert_eq!(
            CredentialPurpose::ProviderAdapter.validate_usage(&CredentialUsage::ProviderAdapter),
            Ok(()),
            "P1/E1 Provider"
        );
        assert_eq!(
            CredentialPurpose::McpAuthorization.validate_usage(&CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },),
            Ok(()),
            "P1/E1 MCP"
        );
        assert_eq!(
            CredentialPurpose::RemoteAgentAuthorization.validate_usage(
                &CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
            ),
            Ok(()),
            "P1/E1 remote Agent"
        );
        assert_eq!(
            CredentialPurpose::WebProviderAuthorization.validate_usage(
                &CredentialUsage::HttpHeader {
                    name: "x-api-key".into(),
                    scheme: None,
                },
            ),
            Ok(()),
            "P1/E1 Web provider"
        );
        assert_eq!(
            validate_credential_target_usage(None, &CredentialUsage::HttpBasicAuth),
            Err(CredentialDescriptorError::MissingTarget),
            "P3/E3 target-dependent access"
        );
        assert_eq!(
            validate_credential_target_usage(None, &CredentialUsage::ProviderAdapter),
            Err(CredentialDescriptorError::MissingTarget),
            "P3/E3 Provider target"
        );
        assert_eq!(
            validate_credential_target_usage(
                None,
                &CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
            ),
            Err(CredentialDescriptorError::MissingTarget),
            "P3/E3 HTTP authorization target"
        );
        assert_eq!(
            CredentialPurpose::HttpEffect.validate_usage(&signature),
            Err(CredentialDescriptorError::InvalidTargetUsage),
            "P2/E2 signature is not an outbound effect"
        );
        assert_eq!(
            validate_credential_target_usage(None, &signature),
            Err(CredentialDescriptorError::MissingTarget),
            "P3/E3 signature access requires an exact consumer target"
        );
    }

    /// Signature descriptor cause/effect table: D1 exact target, field,
    /// algorithm, and one scalar secret => admitted; D2 another algorithm or
    /// target => TargetMismatch; D3 the same scalar declared as two fields =>
    /// material-usage mismatch at the Vault write boundary.
    #[test]
    fn signature_descriptor_binds_one_exact_verification_contract() {
        let target = CredentialTarget::new(
            CredentialPurpose::SignatureVerification,
            "awaken-flow://connector/github/changes/signing",
        );
        let usage = CredentialUsage::SignatureVerification {
            fields: BTreeMap::from([(
                "webhook_secret".into(),
                BTreeSet::from([crate::SignatureVerificationAlgorithm::HmacSha256]),
            )]),
        };
        let descriptor = CredentialDescriptor::new(
            "github",
            CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE),
            [CredentialTargetContract::new(target.clone(), usage.clone())],
        );
        let material = CredentialMaterial::secret(RedactedString::new("secret"));
        assert_eq!(descriptor.admit(&target, &usage), Ok(()), "D1");
        assert_eq!(descriptor.validate_material(&material), Ok(()), "D1");
        assert_eq!(material.validate_usage(&usage), Ok(()), "D1");

        let drifted = CredentialUsage::SignatureVerification {
            fields: BTreeMap::from([(
                "webhook_secret".into(),
                BTreeSet::from([crate::SignatureVerificationAlgorithm::ConstantTime]),
            )]),
        };
        assert_eq!(
            descriptor.admit(&target, &drifted),
            Err(CredentialDescriptorError::TargetMismatch),
            "D2"
        );
        let widened = CredentialUsage::SignatureVerification {
            fields: BTreeMap::from([
                (
                    "token".into(),
                    BTreeSet::from([crate::SignatureVerificationAlgorithm::ConstantTime]),
                ),
                (
                    "webhook_secret".into(),
                    BTreeSet::from([crate::SignatureVerificationAlgorithm::HmacSha256]),
                ),
            ]),
        };
        assert_eq!(
            material.validate_usage(&widened),
            Err(crate::CredentialMaterialError::MaterialKindMismatch),
            "D3"
        );
    }
}
