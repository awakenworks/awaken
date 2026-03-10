use super::agent_definition::AgentDefinition;
use crate::composition::{AgentRegistry, InMemoryAgentRegistry};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
}

impl AgentDescriptor {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            description: String::new(),
        }
    }

    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    #[must_use]
    pub fn into_local(self) -> ResolvedAgent {
        ResolvedAgent {
            descriptor: self,
            binding: AgentBinding::Local,
        }
    }

    #[must_use]
    pub fn into_a2a(self, binding: A2aAgentBinding) -> ResolvedAgent {
        ResolvedAgent {
            descriptor: self,
            binding: AgentBinding::A2a(binding),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AgentDefinitionSpec {
    Local(Box<AgentDefinition>),
    Remote(RemoteAgentDefinition),
}

impl AgentDefinitionSpec {
    #[must_use]
    pub fn local(definition: AgentDefinition) -> Self {
        Self::Local(Box::new(definition))
    }

    #[must_use]
    pub fn local_with_id(agent_id: impl Into<String>, mut definition: AgentDefinition) -> Self {
        definition.id = agent_id.into();
        Self::local(definition)
    }

    #[must_use]
    pub fn a2a(descriptor: AgentDescriptor, binding: A2aAgentBinding) -> Self {
        Self::Remote(RemoteAgentDefinition::a2a(descriptor, binding))
    }

    #[must_use]
    pub fn a2a_with_id(agent_id: impl Into<String>, binding: A2aAgentBinding) -> Self {
        Self::a2a(AgentDescriptor::new(agent_id), binding)
    }

    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Local(definition) => &definition.id,
            Self::Remote(definition) => definition.id(),
        }
    }

    #[must_use]
    pub fn descriptor(&self) -> AgentDescriptor {
        match self {
            Self::Local(definition) => definition.descriptor(),
            Self::Remote(definition) => definition.descriptor(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteAgentDefinition {
    pub descriptor: AgentDescriptor,
    pub binding: RemoteAgentBinding,
}

impl RemoteAgentDefinition {
    #[must_use]
    pub fn a2a(descriptor: AgentDescriptor, binding: A2aAgentBinding) -> Self {
        Self {
            descriptor,
            binding: RemoteAgentBinding::A2a(binding),
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.descriptor.id
    }

    #[must_use]
    pub fn descriptor(&self) -> AgentDescriptor {
        self.descriptor.clone()
    }

    #[must_use]
    pub fn into_resolved_agent(self) -> ResolvedAgent {
        match self.binding {
            RemoteAgentBinding::A2a(binding) => self.descriptor.into_a2a(binding),
        }
    }
}

#[derive(Clone)]
pub struct ResolvedAgent {
    pub descriptor: AgentDescriptor,
    pub binding: AgentBinding,
}

impl ResolvedAgent {
    #[must_use]
    pub fn local(id: impl Into<String>) -> Self {
        AgentDescriptor::new(id).into_local()
    }

    #[must_use]
    pub fn a2a(
        id: impl Into<String>,
        base_url: impl Into<String>,
        remote_agent_id: impl Into<String>,
    ) -> Self {
        AgentDescriptor::new(id).into_a2a(A2aAgentBinding::new(base_url, remote_agent_id))
    }

    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.descriptor.name = name.into();
        self
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.descriptor.description = description.into();
        self
    }

    #[must_use]
    pub fn kind_tag(&self) -> &'static str {
        self.binding.kind_tag()
    }

    #[must_use]
    pub fn is_resumable(&self) -> bool {
        self.binding.is_resumable()
    }
}

impl std::fmt::Debug for ResolvedAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedAgent")
            .field("descriptor", &self.descriptor)
            .field("binding", &self.binding)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub enum RemoteAgentBinding {
    A2a(A2aAgentBinding),
}

#[derive(Debug, Clone)]
pub enum AgentBinding {
    Local,
    A2a(A2aAgentBinding),
}

impl AgentBinding {
    #[must_use]
    pub fn kind_tag(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::A2a(_) => "remote_a2a",
        }
    }

    #[must_use]
    pub fn is_resumable(&self) -> bool {
        matches!(self, Self::Local)
    }
}

#[derive(Clone)]
pub struct A2aAgentBinding {
    pub base_url: String,
    pub remote_agent_id: String,
    pub auth: Option<RemoteSecurityConfig>,
    pub poll_interval_ms: u64,
}

impl A2aAgentBinding {
    #[must_use]
    pub fn new(base_url: impl Into<String>, remote_agent_id: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            remote_agent_id: remote_agent_id.into(),
            auth: None,
            poll_interval_ms: 500,
        }
    }

    #[must_use]
    pub fn with_auth(mut self, auth: RemoteSecurityConfig) -> Self {
        self.auth = Some(auth);
        self
    }

    #[must_use]
    pub fn with_poll_interval_ms(mut self, poll_interval_ms: u64) -> Self {
        self.poll_interval_ms = poll_interval_ms.max(50);
        self
    }
}

impl std::fmt::Debug for A2aAgentBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aAgentBinding")
            .field("base_url", &self.base_url)
            .field("remote_agent_id", &self.remote_agent_id)
            .field("auth", &self.auth)
            .field("poll_interval_ms", &self.poll_interval_ms)
            .finish()
    }
}

#[derive(Clone)]
pub enum RemoteSecurityConfig {
    BearerToken(String),
    Header { name: String, value: String },
}

impl std::fmt::Debug for RemoteSecurityConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BearerToken(_) => f.write_str("BearerToken([redacted])"),
            Self::Header { name, .. } => f
                .debug_struct("Header")
                .field("name", name)
                .field("value", &"[redacted]")
                .finish(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentCatalogError {
    #[error("agent id already registered: {0}")]
    AgentIdConflict(String),
}

pub trait AgentCatalog: Send + Sync {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, id: &str) -> Option<ResolvedAgent>;

    fn descriptor(&self, id: &str) -> Option<AgentDescriptor> {
        self.get(id).map(|agent| agent.descriptor)
    }

    fn ids(&self) -> Vec<String>;

    fn snapshot(&self) -> HashMap<String, ResolvedAgent>;

    fn descriptors(&self) -> HashMap<String, AgentDescriptor> {
        self.snapshot()
            .into_iter()
            .map(|(id, agent)| (id, agent.descriptor))
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryAgentCatalog {
    agents: HashMap<String, ResolvedAgent>,
}

impl InMemoryAgentCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        agent_id: impl Into<String>,
        mut agent: ResolvedAgent,
    ) -> Result<(), AgentCatalogError> {
        let agent_id = agent_id.into();
        if self.agents.contains_key(&agent_id) {
            return Err(AgentCatalogError::AgentIdConflict(agent_id));
        }
        agent.descriptor.id = agent_id.clone();
        if agent.descriptor.name.trim().is_empty() {
            agent.descriptor.name = agent_id.clone();
        }
        self.agents.insert(agent_id, agent);
        Ok(())
    }

    pub fn upsert(&mut self, agent_id: impl Into<String>, mut agent: ResolvedAgent) {
        let agent_id = agent_id.into();
        agent.descriptor.id = agent_id.clone();
        if agent.descriptor.name.trim().is_empty() {
            agent.descriptor.name = agent_id.clone();
        }
        self.agents.insert(agent_id, agent);
    }

    pub fn extend_upsert(&mut self, agents: HashMap<String, ResolvedAgent>) {
        for (id, agent) in agents {
            self.upsert(id, agent);
        }
    }

    pub fn extend_catalog(&mut self, other: &dyn AgentCatalog) -> Result<(), AgentCatalogError> {
        for (id, agent) in other.snapshot() {
            self.register(id, agent)?;
        }
        Ok(())
    }
}

impl AgentCatalog for InMemoryAgentCatalog {
    fn len(&self) -> usize {
        self.agents.len()
    }

    fn get(&self, id: &str) -> Option<ResolvedAgent> {
        self.agents.get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.agents.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn snapshot(&self) -> HashMap<String, ResolvedAgent> {
        self.agents.clone()
    }
}

#[derive(Clone)]
pub struct HostedAgentCatalog {
    agents: Arc<dyn AgentRegistry>,
}

impl HostedAgentCatalog {
    #[must_use]
    pub fn new(agents: Arc<dyn AgentRegistry>) -> Self {
        Self { agents }
    }
}

impl std::fmt::Debug for HostedAgentCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostedAgentCatalog")
            .field("len", &self.agents.len())
            .finish()
    }
}

impl AgentCatalog for HostedAgentCatalog {
    fn len(&self) -> usize {
        self.agents.len()
    }

    fn get(&self, id: &str) -> Option<ResolvedAgent> {
        self.agents
            .get(id)
            .map(|definition| definition.descriptor().into_local())
    }

    fn ids(&self) -> Vec<String> {
        self.agents.ids()
    }

    fn snapshot(&self) -> HashMap<String, ResolvedAgent> {
        self.agents
            .snapshot()
            .into_iter()
            .map(|(id, definition)| (id, definition.descriptor().into_local()))
            .collect()
    }
}

impl AgentCatalog for InMemoryAgentRegistry {
    fn len(&self) -> usize {
        AgentRegistry::len(self)
    }

    fn get(&self, id: &str) -> Option<ResolvedAgent> {
        AgentRegistry::get(self, id).map(|definition| definition.descriptor().into_local())
    }

    fn ids(&self) -> Vec<String> {
        AgentRegistry::ids(self)
    }

    fn snapshot(&self) -> HashMap<String, ResolvedAgent> {
        AgentRegistry::snapshot(self)
            .into_iter()
            .map(|(id, definition)| (id, definition.descriptor().into_local()))
            .collect()
    }
}

#[derive(Clone, Default)]
pub struct CompositeAgentCatalog {
    catalogs: Vec<Arc<dyn AgentCatalog>>,
    cached_snapshot: Arc<RwLock<HashMap<String, ResolvedAgent>>>,
}

impl CompositeAgentCatalog {
    pub fn try_new(
        catalogs: impl IntoIterator<Item = Arc<dyn AgentCatalog>>,
    ) -> Result<Self, AgentCatalogError> {
        let catalogs: Vec<Arc<dyn AgentCatalog>> = catalogs.into_iter().collect();
        let merged = Self::merge_snapshots(&catalogs)?;
        Ok(Self {
            catalogs,
            cached_snapshot: Arc::new(RwLock::new(merged)),
        })
    }

    fn merge_snapshots(
        catalogs: &[Arc<dyn AgentCatalog>],
    ) -> Result<HashMap<String, ResolvedAgent>, AgentCatalogError> {
        let mut merged = InMemoryAgentCatalog::new();
        for catalog in catalogs {
            merged.extend_catalog(catalog.as_ref())?;
        }
        Ok(merged.snapshot())
    }

    fn refresh_snapshot(&self) -> Result<HashMap<String, ResolvedAgent>, AgentCatalogError> {
        Self::merge_snapshots(&self.catalogs)
    }

    fn read_cached_snapshot(&self) -> HashMap<String, ResolvedAgent> {
        match self.cached_snapshot.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn write_cached_snapshot(&self, snapshot: HashMap<String, ResolvedAgent>) {
        match self.cached_snapshot.write() {
            Ok(mut guard) => *guard = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        };
    }
}

impl std::fmt::Debug for CompositeAgentCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = match self.cached_snapshot.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f.debug_struct("CompositeAgentCatalog")
            .field("catalogs", &self.catalogs.len())
            .field("len", &snapshot.len())
            .finish()
    }
}

impl AgentCatalog for CompositeAgentCatalog {
    fn len(&self) -> usize {
        self.snapshot().len()
    }

    fn get(&self, id: &str) -> Option<ResolvedAgent> {
        self.snapshot().get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
        ids.sort();
        ids
    }

    fn snapshot(&self) -> HashMap<String, ResolvedAgent> {
        match self.refresh_snapshot() {
            Ok(snapshot) => {
                self.write_cached_snapshot(snapshot.clone());
                snapshot
            }
            Err(_) => self.read_cached_snapshot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composition::InMemoryAgentRegistry;

    #[test]
    fn hosted_agent_catalog_projects_agent_registry_without_mutating_it() {
        let mut agents = InMemoryAgentRegistry::new();
        agents.upsert(
            "worker",
            crate::composition::AgentDefinition::new("mock")
                .with_name("Worker")
                .with_description("Local worker"),
        );
        let catalog = HostedAgentCatalog::new(Arc::new(agents));
        let agent = catalog.get("worker").expect("agent should exist");
        assert_eq!(agent.descriptor.id, "worker");
        assert_eq!(agent.descriptor.name, "Worker");
        assert_eq!(agent.descriptor.description, "Local worker");
        assert_eq!(agent.kind_tag(), "local");
        assert!(agent.is_resumable());
    }

    #[test]
    fn composite_agent_catalog_rejects_duplicate_ids() {
        let mut left = InMemoryAgentCatalog::new();
        left.upsert("worker", ResolvedAgent::local("worker"));
        let mut right = InMemoryAgentCatalog::new();
        right.upsert(
            "worker",
            ResolvedAgent::a2a("worker", "https://example.test/v1/a2a", "remote-worker"),
        );
        let result = CompositeAgentCatalog::try_new(vec![
            Arc::new(left) as Arc<dyn AgentCatalog>,
            Arc::new(right) as Arc<dyn AgentCatalog>,
        ]);
        assert!(matches!(result, Err(AgentCatalogError::AgentIdConflict(id)) if id == "worker"));
    }

    #[test]
    fn agent_definition_spec_keeps_public_descriptor_shape_for_local_and_remote() {
        let local = AgentDefinitionSpec::local_with_id(
            "local-worker",
            crate::composition::AgentDefinition::new("mock")
                .with_name("Local Worker")
                .with_description("Hosted locally"),
        );
        assert_eq!(local.id(), "local-worker");
        assert_eq!(local.descriptor().name, "Local Worker");

        let remote = AgentDefinitionSpec::a2a(
            AgentDescriptor::new("remote-worker")
                .with_name("Remote Worker")
                .with_description("Delegated over A2A"),
            A2aAgentBinding::new("https://example.test/v1/a2a", "remote-worker"),
        );
        assert_eq!(remote.id(), "remote-worker");
        assert_eq!(remote.descriptor().description, "Delegated over A2A");
    }

    #[test]
    fn agent_definition_spec_convenience_constructors_normalize_common_ids() {
        let local = AgentDefinitionSpec::local_with_id(
            "worker",
            crate::composition::AgentDefinition::new("mock"),
        );
        assert_eq!(local.id(), "worker");

        let remote = AgentDefinitionSpec::a2a_with_id(
            "researcher",
            A2aAgentBinding::new("https://example.test/v1/a2a", "remote-researcher"),
        );
        assert_eq!(remote.id(), "researcher");
        assert_eq!(remote.descriptor().name, "researcher");
    }

    #[test]
    fn remote_security_debug_redacts_secret_material() {
        let auth = RemoteSecurityConfig::BearerToken("secret".to_string());
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("redacted"));
    }
}
