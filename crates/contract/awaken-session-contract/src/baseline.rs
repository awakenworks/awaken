//! Immutable Session baseline and its consumed creation intent (ADR-0066 D1).

use crate::env_registry::EnvironmentRevision;
use awaken_credential_contract::CredentialRealizationProfile;

/// Frozen network fact. This is Session state, not a provider request; the Host
/// projects it to the provisioning contract at realization time.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SessionNetworkPolicy {
    Unrestricted,
    Allowlist { hosts: Vec<String> },
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct EnvironmentFingerprint(pub String);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SessionBaselineFingerprint(pub String);

/// Exact normalized Environment facts frozen for one Session.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentSnapshot {
    pub environment_id: String,
    pub revision: EnvironmentRevision,
    pub config_fingerprint: EnvironmentFingerprint,
    /// Canonicalized, network-free sandbox requirement. `network` is the only
    /// reachability authority in this snapshot.
    pub sandbox: serde_json::Value,
    pub network: SessionNetworkPolicy,
    pub credential_realization: CredentialRealizationProfile,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMcpAuthoringContext {
    pub ordered_vault_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionCreationIntent {
    pub environment_id: String,
    pub agent_id: String,
    pub model: String,
    pub runtime: Option<String>,
    pub mcp_authoring: SessionMcpAuthoringContext,
    pub application: ApplicationContributionState,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ApplicationContributionState {
    Required,
    Absent,
    Committed {
        fingerprint: String,
        input: ApplicationSessionInput,
    },
}

/// Boundary command input. Values remain opaque until the Session application
/// compiler maps them to the owning Resource/provisioning/MCP contracts.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApplicationSessionInput {
    #[serde(default)]
    pub mounts: Vec<serde_json::Value>,
    #[serde(default)]
    pub env: Vec<serde_json::Value>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub mcp_inputs: Vec<serde_json::Value>,
    pub network_restriction: Option<SessionNetworkPolicy>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionBaseline {
    pub fingerprint: SessionBaselineFingerprint,
    pub environment: EnvironmentSnapshot,
    pub mcp_authoring: SessionMcpAuthoringContext,
    pub agent_id: String,
    pub model: String,
    pub runtime: Option<String>,
    #[serde(default)]
    pub delegate_ids: Vec<String>,
    #[serde(default)]
    pub skills: Vec<crate::ResolvedSkillBinding>,
    #[serde(default)]
    pub env: Vec<serde_json::Value>,
    #[serde(default)]
    pub prompts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionBaselineState {
    Preparing(SessionCreationIntent),
    Frozen(SessionBaseline),
}

impl SessionBaseline {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn compile(
        environment: EnvironmentSnapshot,
        mcp_authoring: SessionMcpAuthoringContext,
        agent_id: String,
        model: String,
        runtime: Option<String>,
        delegate_ids: Vec<String>,
        skills: Vec<crate::ResolvedSkillBinding>,
        env: Vec<serde_json::Value>,
        prompts: Vec<String>,
    ) -> Self {
        #[derive(serde::Serialize)]
        struct Facts<'a> {
            environment: &'a EnvironmentSnapshot,
            mcp_authoring: &'a SessionMcpAuthoringContext,
            agent_id: &'a str,
            model: &'a str,
            runtime: &'a Option<String>,
            delegate_ids: &'a [String],
            skills: &'a [crate::ResolvedSkillBinding],
            env: &'a [serde_json::Value],
            prompts: &'a [String],
        }
        let fingerprint = SessionBaselineFingerprint(crate::stable_fingerprint(&Facts {
            environment: &environment,
            mcp_authoring: &mcp_authoring,
            agent_id: &agent_id,
            model: &model,
            runtime: &runtime,
            delegate_ids: &delegate_ids,
            skills: &skills,
            env: &env,
            prompts: &prompts,
        }));
        Self {
            fingerprint,
            environment,
            mcp_authoring,
            agent_id,
            model,
            runtime,
            delegate_ids,
            skills,
            env,
            prompts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_contract::{PlaintextBoundary, PlaintextHolder};

    fn environment(revision: u64, network: SessionNetworkPolicy) -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            environment_id: "env_a".into(),
            revision: EnvironmentRevision(revision),
            config_fingerprint: EnvironmentFingerprint(format!("config-{revision}")),
            sandbox: serde_json::json!({}),
            network,
            credential_realization: CredentialRealizationProfile {
                inference_holder: PlaintextHolder::new(
                    PlaintextBoundary::Workload,
                    "awaken.workload.acp",
                ),
                mcp_holder: PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker"),
            },
        }
    }

    fn baseline(environment: EnvironmentSnapshot) -> SessionBaseline {
        SessionBaseline::compile(
            environment,
            SessionMcpAuthoringContext::default(),
            "agent".into(),
            "model".into(),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn baseline_fingerprint_decision_table() {
        // Cause graph: equal normalized facts -> equal fingerprint; changing the
        // Environment revision or normalized network fact -> different fingerprint.
        //
        // | Rule | Same revision | Same network | Effect |
        // |------|---------------|--------------|--------|
        // | B1   | T             | T            | equal  |
        // | B2   | F             | T            | differ |
        // | B3   | T             | F            | differ |
        let original = baseline(environment(1, SessionNetworkPolicy::Unrestricted));
        assert_eq!(
            original.fingerprint,
            baseline(environment(1, SessionNetworkPolicy::Unrestricted)).fingerprint,
            "B1"
        );
        assert_ne!(
            original.fingerprint,
            baseline(environment(2, SessionNetworkPolicy::Unrestricted)).fingerprint,
            "B2"
        );
        assert_ne!(
            original.fingerprint,
            baseline(environment(1, SessionNetworkPolicy::None)).fingerprint,
            "B3"
        );
    }
}
