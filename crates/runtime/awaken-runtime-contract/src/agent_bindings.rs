//! Published Agent bindings consumed by execution adapters.
//!
//! Agent authoring owns the external wire unions. Compilation normalizes the
//! executable subset into this typed contract. The bindings are a first-class
//! part of the immutable resolved configuration; they are never encoded into a
//! free-form plugin JSON section.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};

use crate::snapshot::AgentId;
use serde::{Deserialize, Serialize};

/// Provider-neutral inference controls frozen into an Agent publication.
///
/// These are deliberately separate from `ModelBinding`: the binding is routing
/// identity used for exact candidate and credential lookup, whereas these values
/// tune each call made through that exact route.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<InferenceSpeed>,
}

impl InferenceOptions {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceSpeed {
    Standard,
    Fast,
}

/// One direct HTTP MCP server inherited by Sessions of the published Agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMcpServerBinding {
    #[serde(alias = "id")]
    pub name: String,
    pub url: String,
    /// Exact secret-free credential revision frozen into the publication.
    ///
    /// Runtime may materialize this revision but must never select a different
    /// credential. `None` denotes an intentionally unauthenticated server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<crate::credential::CredentialRef>,
}

/// The normalized, executable subset of Agent integration configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBindings {
    #[serde(default)]
    pub mcp_servers: Vec<AgentMcpServerBinding>,
    #[serde(default)]
    pub skill_ids: Vec<String>,
    /// Published Agent ids this Agent may invoke through `agent_run`.
    #[serde(default)]
    pub delegate_ids: Vec<AgentId>,
}

/// Strongly typed resolved configuration carried by an executable snapshot.
///
/// Agent integration bindings have a fixed schema and therefore live in their
/// own field. Only extension-owned sections remain open JSON because each plugin
/// owns a different schema and validates its own section before execution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResolvedConfiguration {
    #[serde(default)]
    pub agent: AgentBindings,
    /// Typed model-call controls. This is snapshot data, never plugin-owned JSON.
    #[serde(default, skip_serializing_if = "InferenceOptions::is_default")]
    pub inference: InferenceOptions,
    #[serde(default)]
    plugins: BTreeMap<String, serde_json::Value>,
}

impl ResolvedConfiguration {
    #[must_use]
    pub fn new(agent: AgentBindings, plugins: BTreeMap<String, serde_json::Value>) -> Self {
        Self {
            agent,
            inference: InferenceOptions::default(),
            plugins,
        }
    }

    #[must_use]
    pub fn with_inference(mut self, inference: InferenceOptions) -> Self {
        self.inference = inference;
        self
    }

    #[must_use]
    pub fn plugins(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.plugins
    }
}

impl From<BTreeMap<String, serde_json::Value>> for ResolvedConfiguration {
    fn from(plugins: BTreeMap<String, serde_json::Value>) -> Self {
        Self {
            agent: AgentBindings::default(),
            inference: InferenceOptions::default(),
            plugins,
        }
    }
}

impl Deref for ResolvedConfiguration {
    type Target = BTreeMap<String, serde_json::Value>;

    fn deref(&self) -> &Self::Target {
        &self.plugins
    }
}

impl DerefMut for ResolvedConfiguration {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.plugins
    }
}
