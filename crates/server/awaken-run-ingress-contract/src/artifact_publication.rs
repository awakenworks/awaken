//! Claim-bound Session artifact publication contract and wire metadata.

use awaken_resource_contract::FileRecord;
use serde::{Deserialize, Serialize};

use crate::{RunClaim, WorkerIdentity};

pub const ARTIFACT_PUBLICATION_PATH: &str = "/v1/worker/resources/files/artifacts";
pub const ARTIFACT_METADATA_HEADER: &str = "x-awaken-artifact-publication";

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("artifact publication error: {0}")]
pub struct ArtifactPublicationError(String);

impl ArtifactPublicationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Complete facts needed to publish one immutable Session output.
///
/// Embedded compositions may omit `claim`; a remote Worker must carry the exact
/// claim captured before its attempt began so the Coordinator can fence effects.
#[derive(Debug, Clone)]
pub struct ArtifactPublication {
    pub workspace_id: String,
    pub session_id: String,
    pub logical_path: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub claim: Option<RunClaim>,
}

#[async_trait::async_trait]
pub trait ArtifactPublisher: Send + Sync {
    async fn publish(
        &self,
        publication: ArtifactPublication,
    ) -> Result<FileRecord, ArtifactPublicationError>;
}

/// Signed HTTP metadata; bytes remain in the request body to avoid base64
/// expansion of large artifacts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPublicationRequest {
    pub claim: RunClaim,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<WorkerIdentity>,
    pub workspace_id: String,
    pub session_id: String,
    pub logical_path: String,
    pub mime_type: String,
    pub content_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_metadata_wire_is_strict_and_lossless() {
        // Wire FMECA cause/effect table: C1 every authority field is present;
        // C2 a version-skewed/attacker-controlled field is present. Effects: E1
        // lossless claim/scope/digest transport; E2 fail closed before authority
        // checks. Rules W1 C1+!C2=>E1; W2 C1+C2=>E2.
        let request = ArtifactPublicationRequest {
            claim: RunClaim {
                run_id: awaken_agent_contract::agent::run::Id("run-artifact".into()),
                owner: "worker-artifact".into(),
                epoch: 7,
            },
            identity: None,
            workspace_id: "workspace".into(),
            session_id: "session".into(),
            logical_path: "reports/result.txt".into(),
            mime_type: "text/plain".into(),
            content_id: "digest".into(),
        };
        let encoded = serde_json::to_value(&request).expect("W1 encode");
        let decoded: ArtifactPublicationRequest =
            serde_json::from_value(encoded.clone()).expect("W1 decode");
        assert_eq!(decoded.claim, request.claim, "W1");
        assert_eq!(decoded.logical_path, request.logical_path, "W1");
        assert_eq!(decoded.content_id, request.content_id, "W1");

        let mut unknown = encoded;
        unknown
            .as_object_mut()
            .expect("request object")
            .insert("authority_bypass".into(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<ArtifactPublicationRequest>(unknown).is_err(),
            "W2"
        );
    }
}
