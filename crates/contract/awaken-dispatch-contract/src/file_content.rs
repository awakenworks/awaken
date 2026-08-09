//! Claim-bound immutable File content contract and wire values.

use serde::{Deserialize, Serialize};

use crate::{RunClaim, WorkerIdentity};

pub const FILE_CONTENT_PATH: &str = "/v1/worker/resources/files/content";
pub const FILE_CONTENT_DIGEST_HEADER: &str = "x-awaken-file-content-digest";

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("file content source: {0}")]
pub struct FileContentSourceError(String);

impl FileContentSourceError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Resolve one public Workspace File identity to immutable digest and bytes.
///
/// Remote implementations require a claim; an in-process application adapter
/// may ignore it because no Worker trust boundary is crossed.
#[async_trait::async_trait]
pub trait FileContentSource: Send + Sync {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        claim: Option<&RunClaim>,
    ) -> Result<Option<(String, Vec<u8>)>, FileContentSourceError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileContentRequest {
    pub claim: RunClaim,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<WorkerIdentity>,
    pub workspace_id: String,
    pub file_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_content_request_wire_is_strict_and_lossless() {
        // Cause/effect decision table:
        // | Rule | known fields | unknown field | Effect |
        // | W1 | complete | no | lossless request round-trip |
        // | W2 | complete | yes | reject version-skewed authority input |
        let request = FileContentRequest {
            claim: RunClaim {
                run_id: awaken_agent_contract::agent::run::Id("run-file".into()),
                owner: "worker-file".into(),
                epoch: 7,
            },
            identity: None,
            workspace_id: "workspace-file".into(),
            file_id: "file-public".into(),
        };
        let encoded = serde_json::to_value(&request).expect("W1 encode");
        let decoded: FileContentRequest =
            serde_json::from_value(encoded.clone()).expect("W1 decode");
        assert_eq!(decoded.claim, request.claim, "W1 claim");
        assert_eq!(decoded.workspace_id, request.workspace_id, "W1 workspace");
        assert_eq!(decoded.file_id, request.file_id, "W1 file");

        let mut unknown = encoded;
        unknown
            .as_object_mut()
            .expect("request object")
            .insert("compatibility_bypass".into(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<FileContentRequest>(unknown).is_err(),
            "W2"
        );
    }
}
