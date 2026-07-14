use serde::{Deserialize, Serialize};

use crate::plugin::CapabilityBound;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCapabilityCatalog {
    pub catalog_fingerprint: crate::resolved::CatalogFingerprint,
    pub runtime_version: String,
    pub tools: Vec<ToolCapability>,
    pub plugins: Vec<PluginCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCapability {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginCapability {
    pub id: String,
    pub schema_keys: Vec<String>,
    /// JSON Schema for this plugin's config section, derived from its config
    /// type (e.g. via `schemars`). Carried on the capability catalog so a config
    /// frontend can render and validate the section. Absent when the plugin has
    /// no configuration. Advisory only — the authoritative check is a dry-run
    /// `Plugin::resolve_configured`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// The plugin's declared `CapabilityBound`, projected onto the catalog so it
    /// crosses the runtime↔config boundary (ADR-0004 A1/G8, ADR-0055). An
    /// operator overlay reads this ceiling to allow/deny a plugin by what it may
    /// contribute (e.g. a `tool_gate`) *before* a run, without a dry-run resolve —
    /// the declared bound is the ceiling. Defaults to deny-all when a catalog
    /// predates the projection.
    #[serde(default)]
    pub bound: CapabilityBound,
}

pub trait RuntimeCapabilitySource {
    fn runtime_capabilities(&self) -> RuntimeCapabilityCatalog;
}
