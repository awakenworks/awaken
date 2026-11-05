// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Private File reads select either exact native input provenance
// @formal reason: or the owning Session verdict before reaching content bytes.
//! Read exact personal inputs and Session outputs over the original authorities.

use std::sync::Arc;

use awaken_resource_contract::{
    FileApplicationService, FileCatalogError, FileRecord, ResourcePurgeError,
};
use awaken_session_application::SessionAccessError;
use awaken_session_contract::SessionPrivateBinding;

use super::FileHttpState;

struct SelectedFile {
    application: Arc<dyn FileApplicationService>,
    record: FileRecord,
}

impl FileHttpState {
    async fn select_private(
        &self,
        binding: &SessionPrivateBinding,
        workspace: &str,
        id: &str,
    ) -> Result<Option<SelectedFile>, ResourcePurgeError> {
        if binding.workspace_id() != workspace {
            return Ok(None);
        }
        let factory = self
            .private_inputs
            .as_ref()
            .ok_or_else(authority_unavailable)?;
        let personal = factory.for_binding(binding.as_ref().clone());
        if let Some(record) = personal.get(workspace, id).await? {
            if record.id != id
                || record.workspace_id != workspace
                || record.scope_id.is_some()
                || record.private_binding.as_ref() != Some(binding.as_ref())
            {
                return Err(authority_unavailable());
            }
            return Ok(Some(SelectedFile {
                application: personal,
                record,
            }));
        }
        let Some(record) = self.application.get(workspace, id).await? else {
            return Ok(None);
        };
        if record.id != id || record.workspace_id != workspace || record.private_binding.is_some() {
            return Err(authority_unavailable());
        }
        let Some(session_id) = record.scope_id.as_deref() else {
            // An ordinary input cannot substitute for personal provenance.
            return Ok(None);
        };
        let sessions = self.sessions.as_ref().ok_or_else(authority_unavailable)?;
        match sessions
            .authorize_private(binding, &session_id.into())
            .await
        {
            Ok(_) => Ok(Some(SelectedFile {
                application: self.application.clone(),
                record,
            })),
            Err(SessionAccessError::NotFound) => Ok(None),
            Err(SessionAccessError::Unavailable(_) | SessionAccessError::Internal(_)) => {
                Err(authority_unavailable())
            }
        }
    }

    pub(super) async fn get_private(
        &self,
        binding: &SessionPrivateBinding,
        workspace: &str,
        id: &str,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        Ok(self
            .select_private(binding, workspace, id)
            .await?
            .map(|selected| selected.record))
    }

    pub(super) async fn private_bytes(
        &self,
        binding: &SessionPrivateBinding,
        workspace: &str,
        id: &str,
    ) -> Result<Option<(FileRecord, Vec<u8>)>, ResourcePurgeError> {
        let Some(selected) = self.select_private(binding, workspace, id).await? else {
            return Ok(None);
        };
        let bytes = selected.application.bytes(workspace, id).await?;
        if bytes
            .as_ref()
            .is_some_and(|(record, _)| record != &selected.record)
        {
            return Err(authority_unavailable());
        }
        Ok(bytes)
    }
}

fn authority_unavailable() -> ResourcePurgeError {
    FileCatalogError::AuthorityUnavailable {
        detail: "private File authority is unavailable".into(),
    }
    .into()
}
