//! The scope-free Config Service: author and publish Agent configs.
//!
//! `ConfigService` is the config domain's authoring authority — it validates and
//! stores declarative [`awaken_agent_config::AgentConfig`]s in an
//! [`awaken_agent_config::ConfigRegistry`], and on publish compiles one into a
//! content-addressed [`awaken_agent_config::StoredPublication`] before registering
//! the exact immutable snapshot with Coordinator (ADR-0071).
//!
//! Runtime consumes compiled configuration and never edits authoring records.
use std::sync::Arc;

use awaken_config_resolver::AgentInputBindingRepository;
use awaken_executable_agent_contract::ExecutableAgentRegistrar;

use crate::binding_resolver::ModelPublicationResolver;
use crate::credential_reference::CredentialReferenceValidator;
use crate::plugin_validation::PluginPublicationResolver;

#[cfg(test)]
use crate::ConfigPlane;
#[cfg(test)]
use crate::config_routes::{get_config, publish, put_config, request_scope, validate};
#[cfg(test)]
use crate::tool_catalog::{RESERVED_ADMIN_SCOPE, ToolCatalogSource};
#[cfg(test)]
use awaken_agent_config::DEFAULT_SCOPE;
#[cfg(test)]
use awaken_agent_config::ScopedConfigRegistry;
#[cfg(test)]
use axum::extract::{Path, State};
#[cfg(test)]
use axum::http::StatusCode;
#[cfg(test)]
use axum::{Extension, Json};
#[cfg(test)]
use serde_json::json;

mod authoring;
mod lifecycle;
mod publication;
mod publication_build;

/// The config domain service: validate, store, and publish Agent configuration.
///
/// **Authorization-free by design (ADR-0051/0052).** The already-scoped authoring
/// collaborators — a scope-bound [`awaken_agent_config::ConfigRegistry`] (via
/// [`awaken_agent_config::ScopedConfig`]) and the
/// namespace's resolved tool catalog (`&[awaken_runtime_contract::resolved::ToolDescriptor]`) —
/// are passed in per call
/// by the edge ([`crate::ConfigPlane`] and router handlers). Publication also receives one
/// trusted execution Workspace coordinate for registration. It receives no principal,
/// role, policy, token, authorization decision, or Coordinator store.
pub struct ConfigService {
    /// The sole Control-to-Coordinator executable availability boundary.
    pub(crate) registrar: Arc<dyn ExecutableAgentRegistrar>,
    /// Per-agent resource bindings (ADR-0038). When wired, the agent's bound-resource
    /// prompt fragments are appended to its effective system prompt at compile (A3a).
    /// `None` → compilation is byte-identical to an unbound agent.
    pub(crate) resources: Option<Arc<dyn AgentInputBindingRepository>>,
    /// Resolves authored selection into complete ordered model candidates in one
    /// publication read. Required at construction so a config service can never
    /// publish through an implicit host/provider fallback.
    pub(crate) model_publication_resolver: Arc<dyn ModelPublicationResolver>,
    pub(crate) credential_reference_validator: Option<Arc<dyn CredentialReferenceValidator>>,
    pub(crate) plugin_publication_resolvers: Vec<Arc<dyn PluginPublicationResolver>>,
}

#[cfg(test)]
include!("config_service/test_support.rs");

#[cfg(test)]
pub(crate) mod resource_prompt_tests {
    pub(crate) use super::{agent_config, failing_scoped_plane, test_service};
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    include!("config_service/integration_tests.rs");
}
