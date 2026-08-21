use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("snapshot fingerprint is empty or internally inconsistent")]
    FingerprintMismatch,
    #[error("snapshot not found")]
    SnapshotNotFound,
    #[error("invalid delegation publication graph")]
    InvalidDelegationGraph,
}

pub trait RunResolver {
    fn resolve(
        &self,
        snapshot: &crate::snapshot::ExecutableAgentSnapshot,
    ) -> Result<crate::resolved::ResolvedRun, Error>;
}

pub trait AgentSnapshotResolver {
    fn get_snapshot(
        &self,
        id: &crate::snapshot::ExecutableAgentSnapshotId,
    ) -> Result<Option<crate::snapshot::ExecutableAgentSnapshot>, Error>;
}

pub trait AgentSnapshotCatalog {
    fn list_snapshots(&self) -> Vec<crate::snapshot::ExecutableAgentSnapshotId>;
}

/// Immutable execution lookup for published Agent snapshots.
///
/// New Runs resolve the current publication by Agent identity. Recovery resolves
/// the exact immutable publication recorded in the Run's resume ticket, so a
/// later publish cannot change the behavior of an already-created Run.
pub trait PublishedAgentSnapshotSource: Send + Sync {
    fn current(
        &self,
        workspace: &str,
        agent_id: &crate::snapshot::AgentId,
    ) -> Option<crate::snapshot::ExecutableAgentSnapshot>;

    fn exact(
        &self,
        workspace: &str,
        fingerprint: &crate::resolved::CatalogFingerprint,
    ) -> Option<crate::snapshot::ExecutableAgentSnapshot>;

    /// Resolve the publication compiled from one exact authoring revision.
    /// Embedded sources without revision history may leave this unsupported;
    /// their bindings use `source_revision: None` and resolve current once when
    /// the parent Session is constructed.
    fn at_revision(
        &self,
        _workspace: &str,
        _agent_id: &crate::snapshot::AgentId,
        _source_revision: u64,
    ) -> Option<crate::snapshot::ExecutableAgentSnapshot> {
        None
    }
}

/// Resolve one immutable delegation edge. Both publication freezing and Run
/// admission use this function, so exact-revision and recursive-self semantics
/// cannot drift between the Coordinator and a cold Worker.
pub fn resolve_delegate_snapshot(
    parent: &crate::snapshot::ExecutableAgentSnapshot,
    binding: &crate::agent_bindings::AgentDelegateBinding,
    source: Option<&dyn PublishedAgentSnapshotSource>,
    workspace: &str,
) -> Result<crate::snapshot::ExecutableAgentSnapshot, Error> {
    if binding.recursive_self {
        return (binding.agent_id == parent.root_agent_id)
            .then(|| parent.clone())
            .ok_or(Error::InvalidDelegationGraph);
    }
    let source = source.ok_or(Error::SnapshotNotFound)?;
    let snapshot = binding
        .source_revision
        .and_then(|revision| source.at_revision(workspace, &binding.agent_id, revision))
        .or_else(|| {
            binding
                .source_revision
                .is_none()
                .then(|| source.current(workspace, &binding.agent_id))
                .flatten()
        })
        .ok_or(Error::SnapshotNotFound)?;
    if snapshot.root_agent_id != binding.agent_id {
        return Err(Error::InvalidDelegationGraph);
    }
    Ok(snapshot)
}

/// Freeze the complete immutable publication closure reachable from `parent`.
/// The parent itself remains in `RunActivation`; this returns only non-self
/// targets, deduplicated by content fingerprint, including nested delegates.
pub fn freeze_delegation_publications(
    parent: &crate::snapshot::ExecutableAgentSnapshot,
    source: Option<&dyn PublishedAgentSnapshotSource>,
    workspace: &str,
) -> Result<Vec<crate::snapshot::ExecutableAgentSnapshot>, Error> {
    let mut frozen = Vec::new();
    let mut seen = HashSet::new();
    let mut pending = vec![parent.clone()];
    while let Some(owner) = pending.pop() {
        let mut targets = HashSet::new();
        for binding in &owner.resolved_spec.plugin_config.agent.delegates {
            if !targets.insert(binding.agent_id.clone()) {
                return Err(Error::InvalidDelegationGraph);
            }
            let snapshot = resolve_delegate_snapshot(&owner, binding, source, workspace)?;
            if binding.recursive_self || !seen.insert(snapshot.fingerprint.clone()) {
                continue;
            }
            pending.push(snapshot.clone());
            frozen.push(snapshot);
        }
    }
    Ok(frozen)
}

/// Immutable publication source for embedded hosts and tests that do not run the
/// authoring plane. Callers still provide complete executable snapshots; this is
/// not a second config compiler.
pub struct StaticPublishedAgentSnapshots {
    current: HashMap<crate::snapshot::AgentId, crate::snapshot::ExecutableAgentSnapshot>,
    revisions: HashMap<(crate::snapshot::AgentId, u64), crate::snapshot::ExecutableAgentSnapshot>,
    exact: HashMap<crate::resolved::CatalogFingerprint, crate::snapshot::ExecutableAgentSnapshot>,
}

impl StaticPublishedAgentSnapshots {
    pub fn try_new(
        snapshots: impl IntoIterator<Item = crate::snapshot::ExecutableAgentSnapshot>,
    ) -> Result<Self, Error> {
        let mut current: HashMap<
            crate::snapshot::AgentId,
            crate::snapshot::ExecutableAgentSnapshot,
        > = HashMap::new();
        let mut revisions = HashMap::new();
        let mut exact = HashMap::new();
        for snapshot in snapshots {
            if snapshot.root_agent_id.0.is_empty()
                || snapshot.fingerprint.0.is_empty()
                || snapshot.fingerprint != snapshot.resolved_spec.catalog_fingerprint
                || (!snapshot.metadata.is_legacy_default()
                    && snapshot.metadata.fingerprint.0 != snapshot.fingerprint.0)
            {
                return Err(Error::FingerprintMismatch);
            }
            if exact
                .insert(snapshot.fingerprint.clone(), snapshot.clone())
                .is_some()
            {
                return Err(Error::FingerprintMismatch);
            }
            let revision = snapshot.metadata.source.revision;
            if revision > 0 && snapshot.metadata.source.agent_id != snapshot.root_agent_id {
                return Err(Error::FingerprintMismatch);
            }
            if revision == 0 && current.contains_key(&snapshot.root_agent_id) {
                return Err(Error::FingerprintMismatch);
            }
            if revision > 0
                && revisions
                    .insert((snapshot.root_agent_id.clone(), revision), snapshot.clone())
                    .is_some()
            {
                return Err(Error::FingerprintMismatch);
            }
            match current.get(&snapshot.root_agent_id) {
                Some(existing)
                    if existing.metadata.source.revision >= snapshot.metadata.source.revision => {}
                _ => {
                    current.insert(snapshot.root_agent_id.clone(), snapshot);
                }
            }
        }
        Ok(Self {
            current,
            revisions,
            exact,
        })
    }
}

impl PublishedAgentSnapshotSource for StaticPublishedAgentSnapshots {
    fn current(
        &self,
        _workspace: &str,
        agent_id: &crate::snapshot::AgentId,
    ) -> Option<crate::snapshot::ExecutableAgentSnapshot> {
        self.current.get(agent_id).cloned()
    }

    fn exact(
        &self,
        _workspace: &str,
        fingerprint: &crate::resolved::CatalogFingerprint,
    ) -> Option<crate::snapshot::ExecutableAgentSnapshot> {
        self.exact.get(fingerprint).cloned()
    }

    fn at_revision(
        &self,
        _workspace: &str,
        agent_id: &crate::snapshot::AgentId,
        source_revision: u64,
    ) -> Option<crate::snapshot::ExecutableAgentSnapshot> {
        self.revisions
            .get(&(agent_id.clone(), source_revision))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_bindings::{AgentBindings, AgentDelegateBinding};
    use crate::resolved::ModelBinding;
    use crate::snapshot::{
        AgentConfigRevisionRef, AgentId, AgentPublicationVersion, AgentSnapshotFingerprint,
        AgentSnapshotMetadata, ExecutableAgentSnapshot,
    };

    fn published(
        id: &str,
        revision: u64,
        fingerprint: &str,
        delegates: Vec<AgentDelegateBinding>,
    ) -> ExecutableAgentSnapshot {
        ExecutableAgentSnapshot::builder(id)
            .model(ModelBinding::new("test", "model", "native"))
            .fingerprint(fingerprint)
            .metadata(AgentSnapshotMetadata {
                source: AgentConfigRevisionRef {
                    agent_id: AgentId(id.into()),
                    revision,
                },
                publication_version: AgentPublicationVersion(fingerprint.into()),
                resolution: Default::default(),
                fingerprint: AgentSnapshotFingerprint(fingerprint.into()),
            })
            .agent_bindings(AgentBindings {
                delegates,
                ..Default::default()
            })
            .build()
    }

    fn edge(id: &str, revision: u64) -> AgentDelegateBinding {
        AgentDelegateBinding {
            agent_id: AgentId(id.into()),
            source_revision: Some(revision),
            recursive_self: false,
        }
    }

    /// Publication-closure cause/effect decision table and FMECA. Causes: C1 a
    /// two-level exact-revision graph exists; C2 one required publication is
    /// missing; C3 one Agent has multiple historical versions. Effects: E1 all
    /// reachable targets are frozen once; E2 incomplete graphs fail closed; E3
    /// exact revisions remain independently addressable while current selects
    /// the latest. Rules G1=C1=>E1, G2=C1+C2=>E2, G3=C3=>E3. FMECA: omission or
    /// revision substitution has high severity (wrong Agent code); G2/G3 detect
    /// it before a child Run starts, and fingerprint/revision validation prevents
    /// silent recovery through a mutable current lookup.
    #[test]
    fn freezes_nested_exact_revisions_and_rejects_an_incomplete_graph() {
        let writer_v1 = published("writer", 1, "writer-v1", Vec::new());
        let writer_v2 = published("writer", 2, "writer-v2", Vec::new());
        let researcher = published("researcher", 3, "researcher-v3", vec![edge("writer", 1)]);
        let parent = published("parent", 4, "parent-v4", vec![edge("researcher", 3)]);
        let source = StaticPublishedAgentSnapshots::try_new([
            writer_v1.clone(),
            writer_v2.clone(),
            researcher.clone(),
        ])
        .expect("G1/G3 versioned source");

        let frozen = freeze_delegation_publications(&parent, Some(&source), "workspace")
            .expect("G1 complete graph");
        assert_eq!(
            frozen
                .iter()
                .map(|snapshot| snapshot.fingerprint.0.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["researcher-v3", "writer-v1"]),
            "G1/E1"
        );
        assert_eq!(
            source
                .at_revision("workspace", &AgentId("writer".into()), 1)
                .expect("G3 exact v1"),
            writer_v1,
            "G3/E3"
        );
        assert_eq!(
            source
                .current("workspace", &AgentId("writer".into()))
                .expect("G3 current v2"),
            writer_v2,
            "G3/E3"
        );

        let incomplete = StaticPublishedAgentSnapshots::try_new([researcher]).unwrap();
        assert_eq!(
            freeze_delegation_publications(&parent, Some(&incomplete), "workspace"),
            Err(Error::SnapshotNotFound),
            "G2/E2"
        );
    }
}
