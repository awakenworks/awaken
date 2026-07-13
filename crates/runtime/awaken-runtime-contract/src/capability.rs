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

/// An operator overlay over the advertised plugin capabilities: which plugins the
/// operator forbids for a run, evaluated against each plugin's projected
/// [`CapabilityBound`] ceiling (ADR-0055). Fail-closed by construction — an
/// explicit deny or a forbidden high-privilege axis rejects the plugin.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginGovernancePolicy {
    /// Plugin ids the operator denies outright.
    #[serde(default)]
    pub denied_plugin_ids: Vec<String>,
    /// When true, deny any plugin whose bound admits a tool gate — a pre-execution
    /// decision an operator may reserve to first-party plugins only.
    #[serde(default)]
    pub deny_tool_gates: bool,
}

impl PluginGovernancePolicy {
    /// Whether the overlay admits this advertised plugin. Fail-closed: a denied id
    /// or a forbidden tool-gate ceiling rejects it.
    #[must_use]
    pub fn permits(&self, plugin: &PluginCapability) -> bool {
        if self.denied_plugin_ids.iter().any(|id| id == &plugin.id) {
            return false;
        }
        if self.deny_tool_gates && !plugin.bound.tool_gates.is_deny_all() {
            return false;
        }
        true
    }
}

pub trait RuntimeCapabilitySource {
    fn runtime_capabilities(&self) -> RuntimeCapabilityCatalog;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::IdBound;

    fn cap(id: &str, tool_gates: IdBound) -> PluginCapability {
        PluginCapability {
            id: id.to_string(),
            schema_keys: Vec::new(),
            config_schema: None,
            bound: CapabilityBound {
                tool_gates,
                ..Default::default()
            },
        }
    }

    #[test]
    fn default_policy_permits_every_plugin() {
        let policy = PluginGovernancePolicy::default();
        assert!(policy.permits(&cap("memory", IdBound::Exact(vec![]))));
        assert!(policy.permits(&cap("gate-plugin", IdBound::Exact(vec!["g".into()]))));
    }

    #[test]
    fn denied_id_is_rejected() {
        let policy = PluginGovernancePolicy {
            denied_plugin_ids: vec!["rogue".into()],
            ..Default::default()
        };
        assert!(!policy.permits(&cap("rogue", IdBound::Exact(vec![]))));
        assert!(policy.permits(&cap("memory", IdBound::Exact(vec![]))));
    }

    #[test]
    fn deny_tool_gates_rejects_a_gate_bearing_plugin_only() {
        let policy = PluginGovernancePolicy {
            deny_tool_gates: true,
            ..Default::default()
        };
        // A plugin that contributes no tool gate (deny-all ceiling) is admitted.
        assert!(policy.permits(&cap("memory", IdBound::Exact(vec![]))));
        // One whose bound admits a tool gate is rejected.
        assert!(!policy.permits(&cap("fsm", IdBound::Exact(vec!["fsm".into()]))));
        assert!(!policy.permits(&cap("any-gate", IdBound::Any)));
    }
}
