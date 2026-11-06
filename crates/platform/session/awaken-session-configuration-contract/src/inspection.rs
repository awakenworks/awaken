// @formal surface exclude=read_model_contract
// @formal reason: Transient safe projection of original baseline, publication and committed model-operation facts; no mutation or execution authority.
//! Safe read values over the original baseline and immutable publication.
use crate::{EnvironmentSnapshot, SessionBaseline};
use serde::{Deserialize, Serialize};

/// SDK: none — Awaken-only inspection; never a second configuration authority.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfigurationInspection {
    pub baseline_fingerprint: String,
    pub environment: EnvironmentSnapshot,
    pub runtime_placement: awaken_session_types::SessionRuntimePlacement,
    pub mutation_policy: crate::resource::SessionMutationPolicy,
    /// Only Session-local inference controls, never the replacement route's capabilities.
    pub model_override_inference: Option<awaken_runtime_contract::agent_bindings::InferenceOptions>,
    pub additional_instructions: Vec<String>,
    pub environment_variables: Vec<serde_json::Value>,
    pub mounts: Vec<serde_json::Value>,
    /// None means the exact publication is unavailable, including legacy pins.
    pub publication: Option<serde_json::Value>,
}

impl SessionConfigurationInspection {
    /// Project only declared non-secret metadata. Never serialize the baseline
    /// wholesale: environment secret references are execution capabilities.
    pub fn from_baseline(baseline: &SessionBaseline) -> Self {
        use awaken_provisioning_contract::{EnvValue, MountSource};
        Self {
            baseline_fingerprint: baseline.fingerprint.0.clone(),
            environment: baseline.environment.clone(),
            runtime_placement: baseline.runtime_placement,
            mutation_policy: baseline.mutation_policy,
            model_override_inference: baseline.model_override.as_ref().map(|value| value.inference.clone()),
            additional_instructions: baseline.prompts.clone(),
            environment_variables: baseline.env.iter().map(|var| match &var.value {
                EnvValue::Inline { value } => serde_json::json!({"name":var.name,"visibility":var.visibility,"source":"inline","value":value}),
                EnvValue::Secret { .. } => serde_json::json!({"name":var.name,"visibility":var.visibility,"source":"credential","value":"[REDACTED]"}),
            }).collect(),
            mounts: baseline.mounts.iter().map(|mount| {
                let source = match &mount.source {
                    MountSource::Secret { .. } => "credential",
                    MountSource::Inline { .. } | MountSource::InlineBytes { .. } => "inline",
                    _ => "resource",
                };
                serde_json::json!({"mount_id":mount.mount_id,"mount_path":mount.mount_path,"access":mount.access,"lifetime":mount.lifetime,"required":mount.required,"source":source})
            }).collect(),
            publication: None,
        }
    }
}

/// Safe immutable publication projection for Session inspection.
/// SDK: none — no catalog resolution, credentials or mutable configuration.
pub fn publication_inspection(
    snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
) -> serde_json::Value {
    let spec = snapshot.resolved_spec;
    let mut plugins = serde_json::json!(spec.plugin_config.plugins());
    crate::scrub_sensitive_keys(&mut plugins);
    serde_json::json!({
        "fingerprint": snapshot.fingerprint.0,
        "max_steps": spec.max_steps,
        "delegation_limits": spec.delegation_limits,
        "context_policy": spec.context_policy,
        "plugin_ids": spec.plugin_ids,
        "plugin_config": plugins,
        "inference": spec.plugin_config.inference,
        "tool_presentation": spec.tool_presentation,
    })
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use awaken_runtime_contract::{ExecutableAgentSnapshot, resolved::ModelBinding};
    #[test]
    fn publication_inspection_uses_frozen_values_and_shared_credential_scrubbing() {
        // DT-P1 exact snapshot values -> exact controls; DT-P2 sensitive keys
        // at nested/array depth (including structured credentials) -> redacted.
        // Current catalog/defaults are deliberately unavailable to this pure
        // projector; absence cannot resolve to another publication.
        let mut snapshot = ExecutableAgentSnapshot::builder("original")
            .model(ModelBinding::parse("provider", "model", "native").unwrap())
            .build();
        snapshot.resolved_spec.max_steps = Some(19);
        snapshot.resolved_spec.plugin_ids = vec!["memory".into()];
        snapshot.resolved_spec.plugin_config.insert("memory".into(), serde_json::json!({"recall_enabled":true,"nested":[{"api_key":"do-not-show","credentials":{"key":"do-not-show"}}]})); // awaken-allow(secret): synthetic redaction sentinel, not usable credentials
        let view = publication_inspection(snapshot);
        assert_eq!(view["max_steps"], 19);
        assert_eq!(view["plugin_config"]["memory"]["recall_enabled"], true);
        assert!(!view.to_string().contains("do-not-show"));
        assert!(view.get("model_binding").is_none(), "no route capabilities");
    }
}
