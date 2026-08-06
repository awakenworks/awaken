//! Worker HTTP client for claim-fenced Session artifact publication.

use awaken_resource_contract::content_id;
use awaken_run_ingress_contract::{
    ARTIFACT_METADATA_HEADER, ARTIFACT_PUBLICATION_PATH, ArtifactPublication,
    ArtifactPublicationError, ArtifactPublicationRequest, ArtifactPublisher,
};
use awaken_worker_transport_security::WorkerUpstream;
use base64::Engine as _;

#[derive(Clone)]
pub struct HttpArtifactPublisher {
    upstream: WorkerUpstream,
}

impl HttpArtifactPublisher {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self { upstream }
    }
}

#[async_trait::async_trait]
impl ArtifactPublisher for HttpArtifactPublisher {
    async fn publish(
        &self,
        publication: ArtifactPublication,
    ) -> Result<awaken_resource_contract::FileRecord, ArtifactPublicationError> {
        let claim = publication.claim.ok_or_else(|| {
            ArtifactPublicationError::new("remote artifact publication requires a dispatch claim")
        })?;
        let metadata = ArtifactPublicationRequest {
            claim,
            identity: self.upstream.worker_identity().cloned(),
            workspace_id: publication.workspace_id,
            session_id: publication.session_id,
            logical_path: publication.logical_path,
            mime_type: publication.mime_type,
            content_id: content_id(&publication.bytes),
        };
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&metadata)
                .map_err(|error| ArtifactPublicationError::new(error.to_string()))?,
        );
        let request = self
            .upstream
            .http_client()
            .post(format!(
                "{}{ARTIFACT_PUBLICATION_PATH}",
                self.upstream.base_url()
            ))
            .header(ARTIFACT_METADATA_HEADER, encoded)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(publication.bytes);
        let request = self
            .upstream
            .authorize_request("POST", ARTIFACT_PUBLICATION_PATH, request)
            .map_err(ArtifactPublicationError::new)?;
        let response = request
            .send()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            return Err(ArtifactPublicationError::new(format!(
                "artifact authority returned HTTP {status}"
            )));
        }
        response
            .json::<awaken_resource_contract::FileRecord>()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))
    }
}
