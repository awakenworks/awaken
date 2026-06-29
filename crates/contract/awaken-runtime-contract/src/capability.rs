use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCapability {
    pub id: String,
    pub schema_keys: Vec<String>,
    // TODO(ADR-0004 A1, G8): add a serializable `bound` projection of the
    // plugin's declared `CapabilityBound` (derived by dry-run `resolve`), so the
    // operator overlay can allow/deny a plugin by its `tool_gate` / `transforms`
    // / namespace ceiling before a run. Requires `CapabilityBound` to derive
    // `Serialize`/`Deserialize` so it can cross the runtime↔config boundary.
}

pub trait RuntimeCapabilitySource {
    fn runtime_capabilities(&self) -> RuntimeCapabilityCatalog;
}
