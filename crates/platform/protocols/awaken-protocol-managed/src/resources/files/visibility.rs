// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: The public Files adapter uses the Session owner's retained visibility verdict
// @formal reason: before projection, blob access and deletion. It issues no private or execution
// @formal reason: authority; this is a product-boundary classification, not a formal proof claim.
//! Workspace File PEP over the original File application and Session owner.
use awaken_resource_contract::{FileApplicationService, FileRecord, ResourcePurgeError};
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct FileHttpState {
    pub(super) application: Arc<dyn FileApplicationService>,
    pub(super) sessions: Option<Arc<dyn awaken_session_application::SessionRootReads>>,
    pub(super) private_inputs: Option<awaken_resource_application::PrivateFileViewFactory>,
}

impl FileHttpState {
    async fn is_visible(&self, record: &FileRecord) -> Result<bool, ResourcePurgeError> {
        let Some(session_id) = record.scope_id.as_deref() else {
            return Ok(true);
        };
        let Some(sessions) = &self.sessions else {
            return Ok(false);
        };
        sessions
            .workspace_content_is_public(&record.workspace_id, &session_id.into())
            .await
            .map_err(|_| {
                awaken_resource_contract::FileCatalogError::AuthorityUnavailable {
                    detail: "Session content visibility is unavailable".into(),
                }
                .into()
            })
    }

    pub(super) async fn get(
        &self,
        workspace: &str,
        id: &str,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        let Some(record) = self.application.get(workspace, id).await? else {
            return Ok(None);
        };
        Ok(self.is_visible(&record).await?.then_some(record))
    }

    pub(super) async fn list(
        &self,
        workspace: &str,
        scope: Option<&str>,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError> {
        let records = self.application.list(workspace, scope).await?;
        let mut visible = Vec::with_capacity(records.len());
        for record in records {
            if self.is_visible(&record).await? {
                visible.push(record);
            }
        }
        Ok(visible)
    }

    pub(super) async fn bytes(
        &self,
        workspace: &str,
        id: &str,
    ) -> Result<Option<(FileRecord, Vec<u8>)>, ResourcePurgeError> {
        if self.get(workspace, id).await?.is_none() {
            return Ok(None);
        }
        self.application.bytes(workspace, id).await
    }

    pub(super) async fn delete(
        &self,
        workspace: &str,
        id: &str,
        now: u64,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        if self.get(workspace, id).await?.is_none() {
            return Ok(None);
        }
        self.application.delete(workspace, id, now).await
    }
}
