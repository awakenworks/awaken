//! Runtime adapter from the canonical File application to immutable content reads.

use std::sync::Arc;

use awaken_resource_contract::{FileApplicationService, content_id};
use awaken_run_ingress_contract::{FileContentSource, FileContentSourceError, RunClaim};

pub struct ApplicationFileContentSource {
    application: Arc<dyn FileApplicationService>,
}

impl ApplicationFileContentSource {
    #[must_use]
    pub fn new(application: Arc<dyn FileApplicationService>) -> Self {
        Self { application }
    }
}

#[async_trait::async_trait]
impl FileContentSource for ApplicationFileContentSource {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        _claim: Option<&RunClaim>,
    ) -> Result<Option<(String, Vec<u8>)>, FileContentSourceError> {
        let Some((record, bytes)) = self
            .application
            .bytes(workspace_id, file_id)
            .await
            .map_err(|error| FileContentSourceError::new(error.to_string()))?
        else {
            return Ok(None);
        };
        let actual = content_id(&bytes);
        if actual != record.blob_id {
            return Err(FileContentSourceError::new(format!(
                "File `{file_id}` content digest mismatch: expected {}, received {actual}",
                record.blob_id
            )));
        }
        Ok(Some((record.blob_id, bytes)))
    }
}

pub(crate) struct UnavailableFileContentSource;

#[async_trait::async_trait]
impl FileContentSource for UnavailableFileContentSource {
    async fn read(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _claim: Option<&RunClaim>,
    ) -> Result<Option<(String, Vec<u8>)>, FileContentSourceError> {
        Err(FileContentSourceError::new(
            "File application is not configured by the composition root",
        ))
    }
}

#[cfg(test)]
mod tests {
    use awaken_run_ingress_contract::FileContentSource as _;

    #[tokio::test]
    async fn missing_file_application_fails_closed() {
        // Cause/effect graph: C1 no File application was injected; C2 runtime
        // requests immutable bytes. Decision U1: C1+C2 -> explicit error, with
        // no fallback to FileCatalog/FileStore or process-local state.
        let error = super::UnavailableFileContentSource
            .read("workspace", "file", None)
            .await
            .expect_err("U1");
        assert!(error.to_string().contains("not configured"), "U1: {error}");
    }
}
