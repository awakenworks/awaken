//! The frozen Session runtime projection a durable dispatch carries to a cold
//! Worker.
//!
//! The Session owns this payload; dispatch carries it as an opaque envelope
//! string and never decodes it except for rows written before the Session
//! published its MCP credential requirements beside it.

use awaken_credential_contract::{McpCredentialRequirement, McpCredentialRequirementError};
use awaken_session_mcp_contract::StageMcpAttachment;
use serde::{Deserialize, Serialize};

use crate::{EnvironmentSnapshot, SessionToolConfiguration};

/// Why a frozen runtime projection cannot be dispatched.
#[derive(Debug, thiserror::Error)]
pub enum SessionRuntimeDispatchError {
    #[error(transparent)]
    InvalidMcpCredential(#[from] McpCredentialRequirementError),
    #[error("Session runtime projection does not serialize: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Canonical secret-free runtime projection frozen at Session creation.
///
/// The Session aggregate remains the desired-state authority; this is only its
/// dispatch projection. The persisted JSON form is unchanged from when the
/// dispatch contract defined it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRuntimeProjection {
    pub environment: EnvironmentSnapshot,
    /// `None` retains publication inheritance; `Some(default())` explicitly
    /// clears the complete Session tool configuration.
    #[serde(default)]
    pub tools: Option<SessionToolConfiguration>,
    /// `None` is reserved for legacy rows predating MCP effect projection. New
    /// dispatches always carry `Some`, including `Some([])`.
    #[serde(default)]
    pub mcp_stages: Option<Vec<StageMcpAttachment>>,
}

impl SessionRuntimeProjection {
    /// Encode the projection into the one string a dispatch envelope carries.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Restore the projection from a dispatch envelope's string.
    pub fn decode(projection_json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(projection_json)
    }

    /// Validate the projection and encode it for dispatch: the envelope string
    /// and the MCP credential requirements a claiming Worker must satisfy.
    pub fn dispatch(
        &self,
    ) -> Result<(String, Vec<McpCredentialRequirement>), SessionRuntimeDispatchError> {
        let requirements = self.mcp_credential_requirements()?;
        Ok((self.encode()?, requirements))
    }

    /// Validate every MCP attachment's credential pairing and usage and return
    /// the requirements a claiming Worker must satisfy, in stage order.
    /// Credential-free attachments contribute nothing; the first invalid
    /// attachment rejects the whole projection.
    pub fn mcp_credential_requirements(
        &self,
    ) -> Result<Vec<McpCredentialRequirement>, McpCredentialRequirementError> {
        let mut requirements = Vec::new();
        for stage in self.mcp_stages.iter().flatten() {
            if let Some(requirement) = McpCredentialRequirement::from_attachment(
                stage.credential.as_ref(),
                stage.selected_plaintext_holder.as_ref(),
            )? {
                requirements.push(requirement);
            }
        }
        Ok(requirements)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };

    fn environment() -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            environment_id: "environment-a".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: true,
            config_fingerprint: crate::EnvironmentFingerprint("environment-a@1".into()),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: crate::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        }
    }

    fn holder() -> PlaintextHolder {
        PlaintextHolder::new(PlaintextBoundary::Worker, "worker-a")
    }

    fn access(name: &str, usage: CredentialUsage) -> CredentialAccess {
        CredentialAccess::new(
            CredentialRef {
                id: name.into(),
                revision: 1,
            },
            CredentialMaterialSource::ControlPlaneReference,
            usage,
            CredentialExecutionPolicy::exact(holder(), ModelExposurePolicy::Forbidden),
        )
    }

    fn bearer(name: &str) -> CredentialAccess {
        access(
            name,
            CredentialUsage::HttpHeader {
                name: "Authorization".into(),
                scheme: Some("Bearer".into()),
            },
        )
    }

    fn stage(
        name: &str,
        credential: Option<CredentialAccess>,
        selected_plaintext_holder: Option<PlaintextHolder>,
    ) -> StageMcpAttachment {
        let target = awaken_session_mcp_contract::McpTarget::parse_http("https://mcp.example.test")
            .expect("valid MCP target");
        StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: awaken_session_mcp_contract::McpGenerationRef::from_persisted(
                "session-a".into(),
                awaken_session_mcp_contract::McpGeneration::of_bare_target(
                    awaken_session_mcp_contract::McpAttachmentId(name.into()),
                    1,
                    &target,
                ),
                "worker-a".into(),
                1,
                10_000,
            )
            .expect("MCP generation fence epoch is nonzero"),
            realization_id: format!("realization-{name}"),
            stage_idempotency_key: format!("stage-{name}"),
            name: name.into(),
            target,
            prompts_as_skills: false,
            credential,
            selected_plaintext_holder,
        }
    }

    fn projection(mcp_stages: Option<Vec<StageMcpAttachment>>) -> SessionRuntimeProjection {
        SessionRuntimeProjection {
            environment: environment(),
            tools: Some(Default::default()),
            mcp_stages,
        }
    }

    #[test]
    fn runtime_projection_codec_is_lossless_and_fails_closed() {
        // C1 a typed projection; C2 its encoded string; C3 malformed JSON or a
        // missing Environment. E1 encode/decode is lossless; E2 C3 is refused.
        let value = projection(Some(vec![stage(
            "mcp-a",
            Some(bearer("a")),
            Some(holder()),
        )]));
        let encoded = value.encode().expect("C1 encodes");
        assert_eq!(
            SessionRuntimeProjection::decode(&encoded).expect("C2 decodes"),
            value,
            "E1"
        );
        assert!(
            SessionRuntimeProjection::decode("{").is_err(),
            "E2 malformed"
        );
        assert!(
            SessionRuntimeProjection::decode(r#"{"mcp_stages":[]}"#).is_err(),
            "E2 missing Environment"
        );
        let legacy = SessionRuntimeProjection::decode(
            &serde_json::json!({ "environment": environment() }).to_string(),
        )
        .expect("legacy rows omit tools and MCP stages");
        assert_eq!(legacy.tools, None, "legacy tools inherit");
        assert_eq!(legacy.mcp_stages, None, "legacy MCP stages");
    }

    #[test]
    fn dispatch_publishes_the_encoded_projection_with_its_requirements_or_refuses() {
        // D1 a valid projection yields exactly its encoding and requirements;
        // D2 an invalid attachment refuses dispatch with the attachment error.
        let valid = projection(Some(vec![stage(
            "mcp-a",
            Some(bearer("a")),
            Some(holder()),
        )]));
        let (json, requirements) = valid.dispatch().expect("D1");
        assert_eq!(json, valid.encode().expect("D1 encode"), "D1");
        assert_eq!(
            Ok(requirements),
            valid.mcp_credential_requirements(),
            "D1 requirements"
        );
        let invalid = projection(Some(vec![stage("mcp-a", Some(bearer("a")), None)]));
        assert!(
            matches!(
                invalid.dispatch(),
                Err(SessionRuntimeDispatchError::InvalidMcpCredential(
                    McpCredentialRequirementError::UnpairedHolder
                ))
            ),
            "D2"
        );
    }

    #[test]
    fn mcp_credential_requirements_follow_the_attachment_decision_table() {
        // | Rule | stages | effect |
        // | Q1 | legacy None or empty | no requirements |
        // | Q2 | credential-free stage | contributes nothing |
        // | Q3 | paired Bearer stages | one requirement each, in stage order |
        // | Q4 | any unpaired stage | UnpairedHolder rejects the projection |
        // | Q5 | any non-Bearer stage | UnsupportedUsage rejects the projection |
        assert_eq!(
            projection(None).mcp_credential_requirements(),
            Ok(Vec::new()),
            "Q1"
        );
        assert_eq!(
            projection(Some(Vec::new())).mcp_credential_requirements(),
            Ok(Vec::new()),
            "Q1"
        );
        let requirements = projection(Some(vec![
            stage("mcp-a", Some(bearer("a")), Some(holder())),
            stage("mcp-free", None, None),
            stage("mcp-b", Some(bearer("b")), Some(holder())),
        ]))
        .mcp_credential_requirements()
        .expect("Q2/Q3");
        assert_eq!(
            requirements
                .iter()
                .map(|requirement| requirement.credential.credential.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
            "Q2/Q3 stage order"
        );
        assert!(
            requirements
                .iter()
                .all(|requirement| requirement.selected_plaintext_holder == holder()),
            "Q3 exact holder"
        );
        assert_eq!(
            projection(Some(vec![
                stage("mcp-a", Some(bearer("a")), Some(holder())),
                stage("mcp-b", Some(bearer("b")), None),
            ]))
            .mcp_credential_requirements(),
            Err(McpCredentialRequirementError::UnpairedHolder),
            "Q4"
        );
        assert_eq!(
            projection(Some(vec![stage(
                "mcp-a",
                Some(access("a", CredentialUsage::ProviderAdapter)),
                Some(holder()),
            )]))
            .mcp_credential_requirements(),
            Err(McpCredentialRequirementError::UnsupportedUsage),
            "Q5"
        );
    }
}
