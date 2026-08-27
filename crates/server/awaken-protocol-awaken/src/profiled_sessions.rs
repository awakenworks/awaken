//! Explicit transport for complete product-authored Session creation.

use std::collections::BTreeMap;

use awaken_credential_contract::CredentialRef;
use awaken_provisioning_contract::{EnvVar, MountRequirement};
use awaken_session_contract::{
    McpAttachmentOrigin, McpTarget, SessionNetworkPolicy, SessionToolConfiguration,
};
use axum::Router;
use axum::handler::Handler;
use axum::routing::post;
use serde::{Deserialize, Serialize};

/// One already-authorized, secret-free MCP candidate supplied by a product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionMcpAttachment {
    pub name: String,
    pub target: McpTarget,
    #[serde(default)]
    pub prompts_as_skills: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_credential: Option<CredentialRef>,
    pub origin: McpAttachmentOrigin,
}

/// One exact Repository selected by the product for this Session. The
/// credential is a secret-free Vault reference; Session admission turns it into
/// the existing recipient-bound execution pin before persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionRepository {
    pub remote_url: String,
    pub mount_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_commit: Option<String>,
}

/// Product intent projected into Open's neutral immutable mutation policy.
/// The private extension deliberately has no ordinary Managed variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfiledSessionMode {
    WorkUnit,
    Interactive,
}

impl ProfiledSessionMode {
    #[must_use]
    pub const fn mutation_policy(self) -> awaken_session_contract::SessionMutationPolicy {
        match self {
            Self::WorkUnit => awaken_session_contract::SessionMutationPolicy::Frozen,
            Self::Interactive => awaken_session_contract::SessionMutationPolicy::FileResources,
        }
    }
}

/// Complete, strongly typed Session input delivered to Awaken's sole profiled
/// Session composer. Secret material is forbidden; mounts carry only references.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionCreate {
    pub session_id: String,
    pub mode: ProfiledSessionMode,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(default)]
    pub mounts: Vec<MountRequirement>,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Complete Session-local resource inputs. Open resolves these together
    /// with published Agent defaults before the original root insert.
    #[serde(default)]
    pub resource_inputs: Vec<awaken_session_contract::SessionInputAttachment>,
    #[serde(default)]
    pub mcp_attachments: Vec<ProfiledSessionMcpAttachment>,
    #[serde(default)]
    pub repositories: Vec<ProfiledSessionRepository>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_restriction: Option<SessionNetworkPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<SessionToolConfiguration>,
}

/// Minimal projection returned after the canonical Session root is durable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionCreated {
    pub id: String,
    pub metadata: BTreeMap<String, String>,
}

/// Mount a caller-supplied lowering handler under Awaken's extension namespace.
pub fn profiled_session_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/sessions", post(handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::{
        EnvValue, EnvVisibility, MountAccess, MountLifetime, MountSource,
    };

    #[test]
    fn profiled_session_wire_preserves_typed_secret_references() {
        // Cause/effect decision table:
        // R1 typed Secret mount + Repository credential reference + inline
        // environment -> exact round trip;
        // R2 unknown wire field -> reject before the application handler;
        // R3 missing mode -> reject rather than silently selecting ordinary
        // Managed mutation; R4/R5 map the two closed product modes to Open's
        // immutable Frozen/FileResources policies.
        // The protocol never accepts plaintext credential material.
        let request = ProfiledSessionCreate {
            session_id: "session-a".into(),
            mode: ProfiledSessionMode::WorkUnit,
            agent_id: "agent-a".into(),
            source_revision: Some(2),
            environment_id: Some("environment-a".into()),
            mounts: vec![MountRequirement {
                mount_id: "git-credential".into(),
                source: MountSource::Secret {
                    reference: "credential-source-a@2".into(),
                    content_hash: None,
                },
                mount_path: "/run/awaken/git-credential".into(),
                access: MountAccess::ReadOnly,
                lifetime: MountLifetime::PerRun,
                required: true,
            }],
            env: vec![EnvVar {
                name: "GIT_TERMINAL_PROMPT".into(),
                value: EnvValue::Inline { value: "0".into() },
                visibility: EnvVisibility::Process,
            }],
            prompts: Vec::new(),
            resource_inputs: Vec::new(),
            mcp_attachments: Vec::new(),
            repositories: vec![ProfiledSessionRepository {
                remote_url: "https://github.com/acme/repository".into(),
                mount_path: "repository".into(),
                credential: Some(CredentialRef {
                    id: "credential-source-a".into(),
                    revision: 7,
                }),
                initial_branch: Some("main".into()),
                initial_commit: None,
            }],
            network_restriction: Some(SessionNetworkPolicy::Unrestricted),
            title: None,
            metadata: BTreeMap::new(),
            tools: None,
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<ProfiledSessionCreate>(encoded.clone()).unwrap(),
            request,
            "R1"
        );
        assert_eq!(
            encoded["repositories"][0]["credential"]["revision"], 7,
            "R1 exact Repository credential pin"
        );
        let mut missing_mode = encoded.clone();
        missing_mode.as_object_mut().unwrap().remove("mode");
        assert!(
            serde_json::from_value::<ProfiledSessionCreate>(missing_mode).is_err(),
            "R3"
        );
        assert_eq!(
            ProfiledSessionMode::WorkUnit.mutation_policy(),
            awaken_session_contract::SessionMutationPolicy::Frozen,
            "R4"
        );
        assert_eq!(
            ProfiledSessionMode::Interactive.mutation_policy(),
            awaken_session_contract::SessionMutationPolicy::FileResources,
            "R5"
        );
        let mut unknown = encoded;
        unknown["credential"] = serde_json::json!("plaintext");
        assert!(
            serde_json::from_value::<ProfiledSessionCreate>(unknown).is_err(),
            "R2"
        );
    }
}
