//! The runtime capability catalog the Host publishes to configuration.
//!
//! The Host projects each installed plugin's configuration schema and declared
//! [`CapabilityBound`] into this catalog, and configuration validates Agent
//! publications against it before a run. The Runtime SDK never reads it, so it
//! belongs with the executable Agent catalog (ADR-0086 §3).

use awaken_runtime_contract::CatalogFingerprint;
use awaken_runtime_contract::plugin::CapabilityBound;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCapabilityCatalog {
    pub catalog_fingerprint: CatalogFingerprint,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_capability_keeps_its_wire_shape() {
        // Cause/effect: C1 no config schema -> field omitted; C2 a catalog that
        // predates the bound projection -> deny-all default on decode; C3 a
        // full value -> exact round trip. The move out of the Runtime SDK must
        // not change any of the three.
        let bare = PluginCapability {
            id: "p".into(),
            schema_keys: vec!["k".into()],
            config_schema: None,
            bound: CapabilityBound::default(),
        };
        let wire = serde_json::to_value(&bare).unwrap();
        assert!(wire.get("config_schema").is_none(), "C1");
        assert_eq!(wire["id"], "p");
        assert_eq!(wire["schema_keys"], serde_json::json!(["k"]));
        let legacy: PluginCapability =
            serde_json::from_value(serde_json::json!({"id": "p", "schema_keys": ["k"]})).unwrap();
        assert_eq!(legacy, bare, "C2");
        let full = PluginCapability {
            config_schema: Some(serde_json::json!({"type": "object"})),
            ..bare
        };
        let round: PluginCapability =
            serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
        assert_eq!(round, full, "C3");
    }
}
