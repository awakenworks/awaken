// @formal surface obligation=agent_profile.requested_revision_never_follows_mutable_current
// @formal reason: A request-local index over Control's exact durable publication read.
//! One publication read per permitted authoring scope for a Managed request.

use std::collections::BTreeMap;

use awaken_agent_config::{PublicationState, StoredPublication};
use awaken_config_service::{ConfigPlane, RESERVED_ADMIN_SCOPE};
use awaken_protocol_managed::ManagedAgentError;
use awaken_tenancy::ScopeId;

type PublicationIndex = BTreeMap<(String, u64), StoredPublication>;

pub(super) struct ManagedAgentPublications {
    plane: ConfigPlane,
    workspace: String,
    own: PublicationIndex,
    reserved_visible: bool,
    reserved: tokio::sync::OnceCell<PublicationIndex>,
}

impl ManagedAgentPublications {
    pub(super) async fn read(
        plane: &ConfigPlane,
        workspace: &str,
        reserved_visible: bool,
    ) -> Result<Self, ManagedAgentError> {
        let rows = plane
            .publications(&ScopeId::from(workspace))
            .await
            .map_err(ManagedAgentError::Storage)?;
        Ok(Self {
            plane: plane.clone(),
            workspace: workspace.into(),
            own: Self::index(rows, workspace),
            reserved_visible: reserved_visible && workspace != RESERVED_ADMIN_SCOPE,
            reserved: tokio::sync::OnceCell::new(),
        })
    }

    fn index(rows: Vec<StoredPublication>, workspace: &str) -> PublicationIndex {
        let mut index = BTreeMap::new();
        for publication in rows.into_iter().filter(|row| {
            row.state == PublicationState::Published && row.targets_execution_workspace(workspace)
        }) {
            // Preserve the original first durable publication for a source
            // revision, including historical
            // duplicate fingerprints; never infer a newer mutable revision.
            index
                .entry((publication.agent_id.clone(), publication.source_revision))
                .or_insert(publication);
        }
        index
    }

    pub(super) async fn get(
        &self,
        agent_id: &str,
        revision: u64,
    ) -> Result<Option<&StoredPublication>, ManagedAgentError> {
        let key = (agent_id.to_owned(), revision);
        if let Some(publication) = self.own.get(&key) {
            return Ok(Some(publication));
        }
        if !self.reserved_visible {
            return Ok(None);
        }
        // Preserve own-scope precedence and the original lazy fallback: an
        // unrelated reserved record must not make a readable tenant Agent fail.
        // This cell belongs only to this request, including failed/missing reads.
        let reserved = self
            .reserved
            .get_or_try_init(|| async {
                let rows = self
                    .plane
                    .publications(&ScopeId::from(RESERVED_ADMIN_SCOPE))
                    .await
                    .map_err(ManagedAgentError::Storage)?;
                Ok::<_, ManagedAgentError>(Self::index(rows, &self.workspace))
            })
            .await?;
        Ok(reserved.get(&key))
    }
}
