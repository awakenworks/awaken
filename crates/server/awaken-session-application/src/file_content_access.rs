// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Observes original Session ownership through its selected read-only repository.
// @formal reason: Private source and reader bindings must match; this observation grants no Run,
// @formal reason: Worker or human authority and is not a formal proof of the external ingress.
//! Native File provenance observation, available before executor construction.
use std::sync::Arc;

use awaken_resource_contract::{FileContentSourceError, FileContentVisibility};
use awaken_session_contract::{
    PersistedSession, SessionPrivateBinding, SessionRepositoryError, SessionRepositoryReads,
};
use awaken_session_types::SessionId;

pub struct SessionFileContentAccess {
    repository: Arc<dyn SessionRepositoryReads>,
}

impl SessionFileContentAccess {
    #[must_use]
    pub fn new(repository: Arc<dyn SessionRepositoryReads>) -> Self {
        Self { repository }
    }

    async fn root_in_workspace(
        &self,
        workspace: &str,
        session: &str,
    ) -> Result<Option<PersistedSession>, FileContentSourceError> {
        let id = SessionId::from(session);
        match self.repository.owner(&id).await {
            Ok(owner) if owner == workspace => {}
            Ok(_) | Err(SessionRepositoryError::NotFound) => return Ok(None),
            Err(_) => return Err(unavailable()),
        }
        match self.repository.get(&id).await {
            Ok(root)
                if root.session_id == id
                    && SessionPrivateBinding::of(&root)
                        .is_none_or(|binding| binding.workspace_id() == workspace) =>
            {
                Ok(Some(root))
            }
            Ok(_) => Err(unavailable()),
            Err(SessionRepositoryError::NotFound) => Ok(None),
            Err(_) => Err(unavailable()),
        }
    }
}

fn unavailable() -> FileContentSourceError {
    FileContentSourceError::new("Session File visibility authority is unavailable")
}

#[async_trait::async_trait]
impl FileContentVisibility for SessionFileContentAccess {
    async fn file_content_is_visible(
        &self,
        workspace_id: &str,
        source_session_id: Option<&str>,
        reader_session_id: Option<&str>,
    ) -> Result<bool, FileContentSourceError> {
        let reader_binding = match reader_session_id {
            Some(id) => {
                let Some(reader) = self.root_in_workspace(workspace_id, id).await? else {
                    return Ok(false);
                };
                SessionPrivateBinding::of(&reader).cloned()
            }
            None => None,
        };
        let Some(source_id) = source_session_id else {
            return Ok(reader_binding.is_none());
        };
        if self
            .repository
            .workspace_content_is_public(workspace_id, &source_id.into())
            .await
            .map_err(|_| unavailable())?
        {
            // S has no implicit shared/unknown legacy content for a private reader.
            return Ok(reader_binding.is_none());
        }
        let Some(reader_binding) = reader_binding else {
            return Ok(false);
        };
        let Some(source) = self.root_in_workspace(workspace_id, source_id).await? else {
            return Ok(false);
        };
        Ok(SessionPrivateBinding::of(&source) == Some(&reader_binding))
    }
}
