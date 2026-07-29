//! Composition-time wiring for the config application service.
//!
//! These builders attach ports owned by sibling bounded contexts. Operational
//! authoring, publication, and projection behavior remains in its focused module.

use std::sync::Arc;

use awaken_config_resolver::AgentInputBindingRepository;

use crate::{ConfigService, CredentialReferenceValidator, PluginPublicationResolver};

impl ConfigService {
    /// Wire the per-Agent input binding repository used by Session projections.
    /// Resource inputs are composed with temporary attachments once per Session;
    /// they are deliberately not copied into the Agent snapshot.
    #[must_use]
    pub fn with_resources(mut self, resources: Arc<dyn AgentInputBindingRepository>) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Wire the control-plane adapter that validates exact credential revisions
    /// before an immutable publication is committed.
    #[must_use]
    pub fn with_credential_reference_validator(
        mut self,
        validator: Arc<dyn CredentialReferenceValidator>,
    ) -> Self {
        self.credential_reference_validator = Some(validator);
        self
    }

    /// Install the extension-owned semantic configuration catalog used by both
    /// validate and publish. JSON Schema remains discovery-only.
    #[must_use]
    pub fn with_plugin_publication_resolver(
        mut self,
        resolver: Arc<dyn PluginPublicationResolver>,
    ) -> Self {
        self.plugin_publication_resolvers.push(resolver);
        self
    }
}
