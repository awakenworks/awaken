//! Worker HTTP client for claim-bound immutable File content.

use awaken_resource_contract::content_id;
use awaken_run_ingress_contract::{
    FILE_CONTENT_DIGEST_HEADER, FILE_CONTENT_PATH, FileContentRequest, FileContentSource,
    FileContentSourceError, RunClaim,
};
use awaken_worker_transport_security::WorkerUpstream;

#[derive(Clone)]
pub struct HttpFileContentSource {
    upstream: WorkerUpstream,
}

impl HttpFileContentSource {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self { upstream }
    }
}

#[async_trait::async_trait]
impl FileContentSource for HttpFileContentSource {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        claim: Option<&RunClaim>,
    ) -> Result<Option<(String, Vec<u8>)>, FileContentSourceError> {
        let claim = claim.ok_or_else(|| {
            FileContentSourceError::new("remote File materialization requires a dispatch claim")
        })?;
        if workspace_id.trim().is_empty() || file_id.trim().is_empty() {
            return Err(FileContentSourceError::new(
                "Workspace and File identities must not be empty",
            ));
        }
        let request = self
            .upstream
            .http_client()
            .post(format!("{}{FILE_CONTENT_PATH}", self.upstream.base_url()))
            .json(&FileContentRequest {
                claim: claim.clone(),
                identity: self.upstream.worker_identity().cloned(),
                workspace_id: workspace_id.to_owned(),
                file_id: file_id.to_owned(),
            });
        let request = self
            .upstream
            .authorize_request("POST", FILE_CONTENT_PATH, request)
            .map_err(FileContentSourceError::new)?;
        let response = request
            .send()
            .await
            .map_err(|error| FileContentSourceError::new(error.to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(FileContentSourceError::new(format!(
                "File content authority returned HTTP {}",
                response.status()
            )));
        }
        let digest = response
            .headers()
            .get(FILE_CONTENT_DIGEST_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| FileContentSourceError::new("File response has no content digest"))?
            .to_string();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| FileContentSourceError::new(error.to_string()))?
            .to_vec();
        let actual = content_id(&bytes);
        if actual != digest {
            return Err(FileContentSourceError::new(format!(
                "File response digest mismatch: expected {digest}, received {actual}"
            )));
        }
        Ok(Some((digest, bytes)))
    }
}
