// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: Static owner: executable-only plugin and toolset publication projection. Dynamic path:
// @formal reason: compilation removes inactive authoring residue and validates selected toolsets before
// @formal reason: the immutable Agent fingerprint is minted. Evidence:
// @formal reason: control.agent_profiles.immutable_profile and adjacent decision-table tests; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Canonical executable-only projections used by Agent publication.

use crate::config::AgentConfig;

pub(super) fn plugin_config(
    config: &AgentConfig,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    let active = config
        .plugin_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let acp_backend = config
        .model_binding
        .resolved()
        .is_some_and(|binding| binding.identity().2.starts_with("acp:"));
    config
        .plugin_config
        .iter()
        .filter(|(id, _)| {
            active.contains(id.as_str())
                // ACP has a typed backend-owned section rather than a Runtime
                // Plugin manifest. It is executable only for an ACP route.
                || (id.as_str() == "acp" && acp_backend)
                // Historical permission policy is consumed by the Host's one
                // authorization projector. It remains executable until that
                // codec migrates to AgentBindings; it is not an inactive plugin.
                || id.as_str() == "permission"
        })
        .map(|(id, value)| (id.clone(), value.clone()))
        .collect()
}
