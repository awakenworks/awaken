//! Narrow projection used to authorize one leased Managed Environment Work.

use std::collections::BTreeMap;

use crate::types::agent::AgentSkill;
use crate::types::resource::{ResourceAccess, SessionResource};

use super::{ManagedState, StateError};

/// The resources frozen into the Session named by a current Work lease.
///
/// This is a request-time projection, not a capability registry: durable truth
/// remains the Session aggregate and the WorkQueue lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedWorkSessionScope {
    pub(crate) workspace_id: String,
    pub(crate) skill_versions: BTreeMap<String, Option<String>>,
    pub(crate) memory_stores: BTreeMap<String, ResourceAccess>,
}

impl ManagedState {
    /// Rehydrate and project the exact Session resources a Work token may use.
    pub(crate) async fn work_session_scope(
        &self,
        session_id: &str,
    ) -> Result<ManagedWorkSessionScope, StateError> {
        self.ensure_session(session_id).await?;
        let workspace_id = self
            .resolve_owner(session_id)
            .await?
            .ok_or(StateError::NotFound)?;
        let session = self.get_session(session_id)?;
        let skill_versions = session
            .agent
            .skills
            .into_iter()
            .map(|skill| match skill {
                AgentSkill::Anthropic { skill_id, version }
                | AgentSkill::Custom { skill_id, version } => (skill_id, version),
            })
            .collect();
        let memory_stores = session
            .resources
            .into_iter()
            .filter_map(|resource| match resource {
                SessionResource::MemoryStore {
                    memory_store_id,
                    access,
                    ..
                } => Some((memory_store_id, access.unwrap_or(ResourceAccess::ReadWrite))),
                SessionResource::File { .. } | SessionResource::GithubRepository { .. } => None,
            })
            .collect();
        Ok(ManagedWorkSessionScope {
            workspace_id,
            skill_versions,
            memory_stores,
        })
    }
}
