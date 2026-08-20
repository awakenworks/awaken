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

/// Complete, strongly typed Session input delivered to Awaken's sole profiled
/// Session composer. Secret material is forbidden; mounts carry only references.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionCreate {
    pub session_id: String,
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
    #[serde(default)]
    pub mcp_attachments: Vec<ProfiledSessionMcpAttachment>,
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
        // R1 typed Secret mount + inline environment -> exact round trip;
        // R2 unknown wire field -> reject before the application handler.
        // The protocol never accepts plaintext credential material.
        let request = ProfiledSessionCreate {
            session_id: "session-a".into(),
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
            mcp_attachments: Vec::new(),
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
        let mut unknown = encoded;
        unknown["credential"] = serde_json::json!("plaintext");
        assert!(
            serde_json::from_value::<ProfiledSessionCreate>(unknown).is_err(),
            "R2"
        );
    }
}
