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
}

pub trait RuntimeCapabilitySource {
    fn runtime_capabilities(&self) -> RuntimeCapabilityCatalog;
}
