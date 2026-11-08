// @formal entrypoint session.transcript_prefix_is_exact_request_only_context_not_durable_truth
// @formal entrypoint session_creation.selected_environment_is_frozen_in_baseline
//! Immutable Session baseline and its consumed creation intent (ADR-0066 D1).

use crate::{SessionMutationPolicy, SessionNetworkPolicy};
use awaken_credential_contract::CredentialRealizationProfile;
use awaken_environment_contract::{EnvironmentPackages, EnvironmentRevision};

/// Exact Environment snapshot admission shared by Session creation and Kani.
/// Every resolved snapshot has a positive revision; an immutable published pin
/// additionally requires equality with the requested revision.
#[must_use]
pub const fn resolved_environment_snapshot_is_exact(
    identity_matches: bool,
    actual_revision: u64,
    required_revision: Option<u64>,
) -> bool {
    identity_matches
        && actual_revision > 0
        && match required_revision {
            Some(required) => required > 0 && actual_revision == required,
            None => true,
        }
}

#[cfg(kani)]
mod kani_proofs {
    use super::resolved_environment_snapshot_is_exact;

    /// @obligation session_creation.selected_environment_is_frozen_in_baseline
    #[kani::proof]
    fn resolved_environment_snapshot_accepts_only_exact_positive_identity_and_revision() {
        let identity_matches: bool = kani::any();
        let actual_revision: u64 = kani::any();
        let has_required_revision: bool = kani::any();
        let required_revision: u64 = kani::any();
        let required = has_required_revision.then_some(required_revision);

        assert_eq!(
            resolved_environment_snapshot_is_exact(identity_matches, actual_revision, required),
            identity_matches
                && actual_revision > 0
                && (!has_required_revision
                    || (required_revision > 0 && actual_revision == required_revision))
        );
    }
}

#[cfg(test)]
use awaken_provisioning_contract::SandboxIdleRetentionMode;
use awaken_provisioning_contract::{SandboxIdleRetentionPolicy, SandboxProvisioning};

use awaken_session_types::SessionRuntimePlacement;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct EnvironmentFingerprint(pub String);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SessionBaselineFingerprint(pub String);

/// Immutable selection of the Agent system instructions presented by one
/// Session. A closed enum preserves the distinction between inheritance, an
/// explicit clear, and a replacement without encoding it in mutable metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum SessionSystemPromptSelection {
    #[default]
    Inherit,
    Clear,
    Replace(String),
}

impl SessionSystemPromptSelection {
    #[must_use]
    pub const fn is_inherit(&self) -> bool {
        matches!(self, Self::Inherit)
    }

    #[must_use]
    pub fn resolve(&self, inherited: Option<String>) -> Option<String> {
        match self {
            Self::Inherit => inherited,
            Self::Clear => None,
            Self::Replace(value) => Some(value.clone()),
        }
    }
}

/// Exact normalized Environment facts frozen for one Session.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSnapshot {
    pub environment_id: String,
    pub revision: EnvironmentRevision,
    /// Whether execution is delegated through the external Worker WorkQueue.
    /// This placement fact is frozen with the Environment revision so recovery
    /// never reopens today's mutable executable catalog.
    pub self_hosted: bool,
    pub config_fingerprint: EnvironmentFingerprint,
    /// Canonicalized, network-free sandbox requirement. `network` is the only
    /// reachability authority in this snapshot.
    pub sandbox: awaken_provisioning_contract::SandboxOverride,
    /// Frozen creation timing from the exact Environment-bound execution policy.
    pub sandbox_provisioning: SandboxProvisioning,
    /// Frozen whole-Environment idle continuation policy.
    pub idle_retention: SandboxIdleRetentionPolicy,
    /// Exact package inputs frozen with this Environment revision. Providers
    /// provision them before workload launch or reject the spec fail-closed.
    pub packages: EnvironmentPackages,
    /// Immutable OCI reference prepared by Coordinator for these exact package inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_image: Option<String>,
    pub network: SessionNetworkPolicy,
    pub credential_realization: CredentialRealizationProfile,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMcpAuthoringContext {
    pub ordered_vault_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlSessionCreationInputs {
    /// Immutable post-create mutation authority. Historical intents decode as
    /// ordinary Managed Sessions; product adapters must select a profiled mode
    /// explicitly before this complete intent is finalized.
    #[serde(default, skip_serializing_if = "SessionMutationPolicy::is_managed")]
    pub mutation_policy: SessionMutationPolicy,
    /// Opaque source evidence; product/hosting verification owns its meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_origin: Option<crate::ApplicationOriginBinding>,
    pub environment: EnvironmentSnapshot,
    pub runtime_placement: SessionRuntimePlacement,
    pub agent_id: String,
    /// Exact immutable Agent publication selected for this Session. Historical
    /// rows without a pin retain `None` and use their legacy recovery behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_revision: Option<u64>,
    /// The Session-scoped publication of an Agent its Workspace does not
    /// publish, frozen at creation (see [`SessionBaseline::inline_agent_publication`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_agent_publication: Option<Box<awaken_runtime_contract::ExecutableAgentSnapshot>>,
    pub model: String,
    pub execution_model_ref: String,
    /// Complete Session-local model semantics when the caller overrides the
    /// Agent model. `None` means the immutable Agent route and inference controls
    /// remain authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<crate::SessionModelOverride>,
    /// Exact Session-local system prompt selection. Historical intents inherit
    /// their pinned Agent publication and retain the legacy serialized shape.
    #[serde(
        default,
        skip_serializing_if = "SessionSystemPromptSelection::is_inherit"
    )]
    pub system_prompt: SessionSystemPromptSelection,
    pub runtime: Option<String>,
    pub mcp_authoring: SessionMcpAuthoringContext,
    pub delegate_ids: Vec<String>,
    pub toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    pub mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub env: Vec<awaken_provisioning_contract::EnvVar>,
    pub prompts: Vec<String>,
    /// Optional immutable committed-history prefix projected into every model
    /// request for this Session. The referenced source Thread remains the sole
    /// transcript authority; no source message is copied into target truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_prefix:
        Option<awaken_agent_contract::thread::read::transcript::TranscriptSliceSpec>,
    pub resources: crate::ResolvedSessionResources,
    pub initial_mcp: Vec<crate::McpAttachmentDraft>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreationIntent {
    pub control: ControlSessionCreationInputs,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledSessionCreation {
    pub baseline: SessionBaseline,
    pub initial_resources: crate::ResolvedSessionResources,
    pub initial_mcp: Vec<crate::McpAttachmentDraft>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionCreationFinalizeError {
    #[error("MCP attachment definitions conflict: {0}")]
    McpConflict(String),
}

impl SessionCreationIntent {
    /// Compile the complete creation intent before any external realization.
    /// Every caller supplies its immutable inputs up front; Worker-local setup
    /// remains an execution concern and cannot mutate the Session baseline.
    pub fn finalize(self) -> Result<CompiledSessionCreation, SessionCreationFinalizeError> {
        let ControlSessionCreationInputs {
            mutation_policy,
            application_origin,
            environment,
            runtime_placement,
            agent_id,
            agent_revision,
            inline_agent_publication,
            model,
            execution_model_ref,
            model_override,
            system_prompt,
            runtime,
            mcp_authoring,
            delegate_ids,
            toolsets,
            mounts,
            env,
            prompts,
            transcript_prefix,
            resources,
            initial_mcp,
        } = self.control;
        let initial_mcp = crate::mcp::resolve_mcp_draft_precedence(initial_mcp)
            .map_err(|error| SessionCreationFinalizeError::McpConflict(error.to_string()))?;
        let baseline = SessionBaseline::compile_with_policy_system_and_execution_model_ref(
            SessionBaselineInputs {
                environment,
                runtime_placement,
                mcp_authoring,
                agent_id,
                agent_revision,
                model,
                model_override,
                runtime,
                delegate_ids,
                toolsets,
                mounts,
                env,
                prompts,
                transcript_prefix,
            },
            execution_model_ref,
            mutation_policy,
            system_prompt,
            application_origin,
        )
        .with_inline_agent_publication(inline_agent_publication);
        Ok(CompiledSessionCreation {
            baseline,
            initial_resources: resources,
            initial_mcp,
        })
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBaseline {
    pub fingerprint: SessionBaselineFingerprint,
    #[serde(default, skip_serializing_if = "SessionMutationPolicy::is_managed")]
    pub mutation_policy: SessionMutationPolicy,
    /// Frozen with the original root, never inferred from mutable metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_origin: Option<crate::ApplicationOriginBinding>,
    pub environment: EnvironmentSnapshot,
    pub runtime_placement: SessionRuntimePlacement,
    pub mcp_authoring: SessionMcpAuthoringContext,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_revision: Option<u64>,
    /// The Session-scoped publication of an Agent its Workspace does not
    /// publish (`agent_revision` is then `None`), frozen at creation so a
    /// Worker-placed Session pins an exact publication. It is immutable: its
    /// content address enters this baseline's fingerprint, it never enters
    /// the Workspace's published-Agent catalog, and it is addressable only
    /// through this Session. Historical and published-Agent baselines omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_agent_publication: Option<Box<awaken_runtime_contract::ExecutableAgentSnapshot>>,
    pub model: String,
    /// Runtime coordinate resolved from `model` by the canonical publication
    /// resolver (the Agent publication or a Session-local override). It is
    /// fingerprinted and never inferred from public syntax after admission.
    pub execution_model_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<crate::SessionModelOverride>,
    #[serde(
        default,
        skip_serializing_if = "SessionSystemPromptSelection::is_inherit"
    )]
    pub system_prompt: Box<SessionSystemPromptSelection>,
    pub runtime: Option<String>,
    pub delegate_ids: Vec<String>,
    pub toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    pub mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub env: Vec<awaken_provisioning_contract::EnvVar>,
    pub prompts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_prefix:
        Option<awaken_agent_contract::thread::read::transcript::TranscriptSliceSpec>,
}

pub struct SessionBaselineInputs {
    pub environment: EnvironmentSnapshot,
    pub runtime_placement: SessionRuntimePlacement,
    pub mcp_authoring: SessionMcpAuthoringContext,
    pub agent_id: String,
    pub agent_revision: Option<u64>,
    pub model: String,
    pub model_override: Option<crate::SessionModelOverride>,
    pub runtime: Option<String>,
    pub delegate_ids: Vec<String>,
    pub toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    pub mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub env: Vec<awaken_provisioning_contract::EnvVar>,
    pub prompts: Vec<String>,
    pub transcript_prefix:
        Option<awaken_agent_contract::thread::read::transcript::TranscriptSliceSpec>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionBaselineState {
    Preparing(SessionCreationIntent),
    Frozen(SessionBaseline),
}

impl SessionBaselineState {
    /// Normalize only a baseline from the recognized pre-envelope Session format.
    /// Current snapshots still require every frozen fact during Deserialize;
    /// callers select the historical format before invoking this whole-shape law.
    /// No fingerprint, source revision or current catalog is reconstructed here.
    pub fn normalize_legacy_baseline(
        baseline: &mut serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), serde_json::Error> {
        use serde::de::Error as _;

        let state = baseline
            .get("state")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde_json::Error::custom("managed Session baseline has no state"))?;
        if state != "frozen" {
            return Err(serde_json::Error::custom(format!(
                "unsupported legacy managed Session baseline state `{state}`"
            )));
        }
        // Application contributions were fully consumed into these frozen fields;
        // the nullable receipt was retired and must not become a second authority.
        baseline.remove("application");
        let environment = baseline
            .get_mut("environment")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| {
                serde_json::Error::custom("managed Session baseline has no environment snapshot")
            })?;
        environment
            .entry("self_hosted")
            .or_insert_with(|| serde_json::json!(false));
        let runtime_placement = if environment
            .get("self_hosted")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| {
                serde_json::Error::custom("Session self_hosted placement is not boolean")
            })? {
            "worker"
        } else {
            "local"
        };
        environment.entry("idle_retention").or_insert_with(|| {
            serde_json::to_value(SandboxIdleRetentionPolicy::default())
                .expect("default idle-retention policy serializes")
        });
        // Missing/null is the supported legacy omission; a present value of
        // another JSON kind is corruption, never an absent placement.
        let existing_placement = match baseline.get("runtime_placement") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(placement)) => Some(placement.as_str()),
            Some(_) => {
                return Err(serde_json::Error::custom(
                    "unknown Session runtime placement",
                ));
            }
        };
        match existing_placement {
            None | Some("legacy_unspecified") => {
                baseline.insert(
                    "runtime_placement".into(),
                    serde_json::json!(runtime_placement),
                );
            }
            Some("local" | "worker") => {}
            Some(_) => {
                return Err(serde_json::Error::custom(
                    "unknown Session runtime placement",
                ));
            }
        }
        Ok(())
    }

    /// The named input-mutation decision frozen with the Session; a Session
    /// still preparing its baseline admits the default policy.
    #[must_use]
    pub fn resource_mutation_policy(&self) -> SessionMutationPolicy {
        match self {
            Self::Frozen(baseline) => baseline.mutation_policy,
            Self::Preparing(_) => SessionMutationPolicy::default(),
        }
    }
}

impl SessionBaseline {
    #[must_use]
    pub fn compile(inputs: SessionBaselineInputs) -> Self {
        let execution_model_ref = inputs.model.clone();
        Self::compile_with_execution_model_ref(inputs, execution_model_ref)
    }

    /// Compile a baseline whose public Managed model id differs from the exact
    /// model coordinate frozen into the executable publication.
    #[must_use]
    pub fn compile_with_execution_model_ref(
        inputs: SessionBaselineInputs,
        execution_model_ref: String,
    ) -> Self {
        Self::compile_with_policy_and_execution_model_ref(
            inputs,
            execution_model_ref,
            SessionMutationPolicy::Managed,
        )
    }

    /// Compile the immutable baseline with an explicit post-create mutation
    /// boundary. The Managed branch intentionally retains the historical
    /// fingerprint byte-for-byte; only profiled policies add a domain-separated
    /// fact.
    #[must_use]
    pub(crate) fn compile_with_policy_and_execution_model_ref(
        inputs: SessionBaselineInputs,
        execution_model_ref: String,
        mutation_policy: SessionMutationPolicy,
    ) -> Self {
        Self::compile_with_policy_system_and_execution_model_ref(
            inputs,
            execution_model_ref,
            mutation_policy,
            SessionSystemPromptSelection::Inherit,
            None,
        )
    }

    #[must_use]
    fn compile_with_policy_system_and_execution_model_ref(
        inputs: SessionBaselineInputs,
        execution_model_ref: String,
        mutation_policy: SessionMutationPolicy,
        system_prompt: SessionSystemPromptSelection,
        application_origin: Option<crate::ApplicationOriginBinding>,
    ) -> Self {
        #[derive(serde::Serialize)]
        struct Facts<'a> {
            environment: &'a EnvironmentSnapshot,
            runtime_placement: SessionRuntimePlacement,
            mcp_authoring: &'a SessionMcpAuthoringContext,
            agent_id: &'a str,
            agent_revision: Option<u64>,
            model: &'a str,
            execution_model_ref: &'a str,
            model_override: &'a Option<crate::SessionModelOverride>,
            runtime: &'a Option<String>,
            delegate_ids: &'a [String],
            toolsets: &'a [awaken_agent_contract::ToolsetPolicy],
            mounts: &'a [awaken_provisioning_contract::MountRequirement],
            env: &'a [awaken_provisioning_contract::EnvVar],
            prompts: &'a [String],
            transcript_prefix:
                &'a Option<awaken_agent_contract::thread::read::transcript::TranscriptSliceSpec>,
        }
        let SessionBaselineInputs {
            environment,
            runtime_placement,
            mcp_authoring,
            agent_id,
            agent_revision,
            model,
            model_override,
            runtime,
            delegate_ids,
            toolsets,
            mounts,
            env,
            prompts,
            transcript_prefix,
        } = inputs;
        let legacy_fingerprint = crate::stable_fingerprint(&Facts {
            environment: &environment,
            runtime_placement,
            mcp_authoring: &mcp_authoring,
            agent_id: &agent_id,
            agent_revision,
            model: &model,
            execution_model_ref: &execution_model_ref,
            model_override: &model_override,
            runtime: &runtime,
            delegate_ids: &delegate_ids,
            toolsets: &toolsets,
            mounts: &mounts,
            env: &env,
            prompts: &prompts,
            transcript_prefix: &transcript_prefix,
        });
        let authoring_fingerprint = if system_prompt.is_inherit() {
            legacy_fingerprint
        } else {
            crate::stable_fingerprint(&(
                "session-system-prompt-selection-v1",
                legacy_fingerprint,
                &system_prompt,
            ))
        };
        let policy_fingerprint = if mutation_policy.is_managed() {
            authoring_fingerprint
        } else {
            crate::stable_fingerprint(&(
                "profiled-session-mutation-policy-v1",
                authoring_fingerprint,
                mutation_policy,
            ))
        };
        let fingerprint = SessionBaselineFingerprint(match &application_origin {
            None => policy_fingerprint,
            Some(origin) => crate::stable_fingerprint(&(
                "session-application-origin-v1",
                policy_fingerprint,
                origin,
            )),
        });
        Self {
            fingerprint,
            mutation_policy,
            application_origin,
            environment,
            runtime_placement,
            mcp_authoring,
            agent_id,
            agent_revision,
            inline_agent_publication: None,
            model,
            execution_model_ref,
            model_override,
            system_prompt: Box::new(system_prompt),
            runtime,
            delegate_ids,
            toolsets,
            mounts,
            env,
            prompts,
            transcript_prefix,
        }
    }

    /// Pin the Session-scoped publication frozen for an unpublished Agent.
    /// Its content address is domain-separated into the fingerprint, so a
    /// baseline without one keeps its historical fingerprint byte-for-byte
    /// and a different frozen publication is a different baseline.
    #[must_use]
    pub fn with_inline_agent_publication(
        mut self,
        publication: Option<Box<awaken_runtime_contract::ExecutableAgentSnapshot>>,
    ) -> Self {
        if let Some(publication) = &publication {
            self.fingerprint = SessionBaselineFingerprint(crate::stable_fingerprint(&(
                "session-inline-agent-publication-v1",
                &self.fingerprint.0,
                &publication.fingerprint.0,
            )));
        }
        self.inline_agent_publication = publication;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::{credential::PlaintextBoundary, credential::PlaintextHolder};
    use awaken_provisioning_contract::{
        EnvValue, EnvVar, EnvVisibility, MountAccess, MountLifetime, MountRequirement, MountSource,
        SandboxCheckpointExpiryBehavior,
    };

    fn mount(id: &str) -> MountRequirement {
        MountRequirement {
            mount_id: id.into(),
            source: MountSource::Inline {
                contents: id.into(),
            },
            mount_path: format!("/workspace/{id}"),
            access: MountAccess::ReadOnly,
            lifetime: MountLifetime::PerRun,
            required: true,
        }
    }

    fn env_var(name: &str) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: EnvValue::Inline { value: "1".into() },
            visibility: EnvVisibility::Process,
        }
    }

    fn environment(revision: u64, network: SessionNetworkPolicy) -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            environment_id: "env_a".into(),
            revision: EnvironmentRevision(revision),
            self_hosted: false,
            config_fingerprint: EnvironmentFingerprint(format!("config-{revision}")),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network,
            credential_realization: CredentialRealizationProfile {
                inference_holder: PlaintextHolder::new(
                    PlaintextBoundary::Workload,
                    "awaken.workload.acp",
                ),
                mcp_holder: PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker"),
                resource_holder: PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker"),
            },
        }
    }

    #[test]
    fn idle_retention_validation_follows_the_decision_table() {
        // Cause/effect graph: Resident ignores checkpoint-only values for legacy
        // compatibility. CheckpointAndRelease requires C1 positive idle delay,
        // C2 retention > idle delay, C3 positive size/time bounds and C4 an exact
        // format. R1 Resident => E1 valid; R2 all C1..C4 => E1 valid; R3..R7 each
        // violate one constraint => E2 reject before a Session can freeze it.
        let resident = SandboxIdleRetentionPolicy::default();
        assert_eq!(resident.validate(), Ok(()), "R1");

        let valid = SandboxIdleRetentionPolicy {
            mode: SandboxIdleRetentionMode::CheckpointAndRelease,
            checkpoint_after_secs: 60,
            retention_secs: 3_600,
            expiry_behavior: SandboxCheckpointExpiryBehavior::FreshFromFrozenEnvironment,
            max_checkpoint_bytes: 1_024,
            max_checkpoint_duration_secs: 30,
            checkpoint_format: "awaken-fs-tar-v1".into(),
        };
        assert_eq!(valid.validate(), Ok(()), "R2");
        for (rule, invalid) in [
            (
                "R3",
                SandboxIdleRetentionPolicy {
                    checkpoint_after_secs: 0,
                    ..valid.clone()
                },
            ),
            (
                "R4",
                SandboxIdleRetentionPolicy {
                    retention_secs: 60,
                    ..valid.clone()
                },
            ),
            (
                "R5",
                SandboxIdleRetentionPolicy {
                    max_checkpoint_bytes: 0,
                    ..valid.clone()
                },
            ),
            (
                "R6",
                SandboxIdleRetentionPolicy {
                    max_checkpoint_duration_secs: 0,
                    ..valid.clone()
                },
            ),
            (
                "R7",
                SandboxIdleRetentionPolicy {
                    checkpoint_format: "  ".into(),
                    ..valid.clone()
                },
            ),
        ] {
            assert!(invalid.validate().is_err(), "{rule}");
        }
    }

    #[test]
    fn environment_snapshot_requires_every_frozen_fact() {
        // Required-field partition: P1 complete snapshot -> exact round trip;
        // P2 missing placement; P3 missing retention; P4 unknown fact -> reject.
        // Recovery cannot invent a placement or lifecycle policy.
        let mut encoded =
            serde_json::to_value(environment(1, SessionNetworkPolicy::Unrestricted)).unwrap();
        encoded.as_object_mut().unwrap().remove("self_hosted");
        assert!(
            serde_json::from_value::<EnvironmentSnapshot>(encoded).is_err(),
            "P2"
        );

        let mut encoded =
            serde_json::to_value(environment(1, SessionNetworkPolicy::Unrestricted)).unwrap();
        encoded.as_object_mut().unwrap().remove("idle_retention");
        assert!(
            serde_json::from_value::<EnvironmentSnapshot>(encoded).is_err(),
            "P3"
        );

        let mut explicit = environment(1, SessionNetworkPolicy::Unrestricted);
        explicit.self_hosted = true;
        let mut encoded = serde_json::to_value(&explicit).unwrap();
        let decoded: EnvironmentSnapshot = serde_json::from_value(encoded.clone()).unwrap();
        assert!(decoded.self_hosted, "P1");
        encoded["unexpected"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<EnvironmentSnapshot>(encoded).is_err(),
            "P4"
        );

        // Legacy decision table L1-L6: idle missing/explicit nondefault crossed
        // with self_hosted missing/false/true. Only the named historical law
        // fills missing retention and derives local/worker placement; explicit
        // policy and the original fingerprint survive. P2/P3/P4 above remain
        // strict current-input controls, not compatibility defaults.
        let baseline = SessionBaselineState::Frozen(SessionBaseline::compile(baseline_inputs(
            environment(1, SessionNetworkPolicy::Unrestricted),
        )));
        let original = serde_json::to_value(&baseline).unwrap();
        let resident = original["environment"]["idle_retention"].clone();
        let mut explicit = resident.clone();
        explicit["retention_secs"] = serde_json::json!(73);
        for policy_present in [false, true] {
            for hosted in [None, Some(false), Some(true)] {
                let mut legacy = original.clone();
                let snapshot = legacy["environment"].as_object_mut().unwrap();
                if policy_present {
                    snapshot.insert("idle_retention".into(), explicit.clone());
                } else {
                    snapshot.remove("idle_retention");
                }
                if let Some(hosted) = hosted {
                    snapshot.insert("self_hosted".into(), serde_json::json!(hosted));
                } else {
                    snapshot.remove("self_hosted");
                }
                legacy.as_object_mut().unwrap().remove("runtime_placement");
                legacy["application"] = serde_json::Value::Null;
                SessionBaselineState::normalize_legacy_baseline(legacy.as_object_mut().unwrap())
                    .unwrap();
                assert_eq!(
                    legacy["environment"]["idle_retention"],
                    if policy_present {
                        explicit.clone()
                    } else {
                        resident.clone()
                    }
                );
                assert_eq!(
                    legacy["environment"]["self_hosted"],
                    hosted.unwrap_or(false)
                );
                assert_eq!(
                    legacy["runtime_placement"],
                    if hosted == Some(true) {
                        "worker"
                    } else {
                        "local"
                    }
                );
                assert_eq!(legacy["fingerprint"], original["fingerprint"]);
                assert!(legacy.get("application").is_none());
                assert!(serde_json::from_value::<SessionBaselineState>(legacy).is_ok());
            }
        }
        // L7-L9 reject unsupported state, nonboolean placement and unknown
        // runtime placement before typed decoding; the original diagnostics stay.
        for (pointer, value, diagnostic) in [
            (
                "/state",
                serde_json::json!("preparing"),
                "unsupported legacy managed Session baseline state `preparing`",
            ),
            (
                "/environment/self_hosted",
                serde_json::json!("false"),
                "Session self_hosted placement is not boolean",
            ),
            (
                "/runtime_placement",
                serde_json::json!("unknown"),
                "unknown Session runtime placement",
            ),
        ] {
            let mut malformed = original.clone();
            *malformed.pointer_mut(pointer).unwrap() = value;
            let failure =
                SessionBaselineState::normalize_legacy_baseline(malformed.as_object_mut().unwrap())
                    .unwrap_err();
            assert_eq!(failure.to_string(), diagnostic);
        }
        // L10-L13: an existing non-string/non-null placement is malformed,
        // distinct from the supported missing/null legacy value. It must not
        // be collapsed into absence and replaced from self_hosted.
        for placement in [
            serde_json::json!(37),
            serde_json::json!(true),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let mut malformed = original.clone();
            malformed["runtime_placement"] = placement;
            assert!(
                SessionBaselineState::normalize_legacy_baseline(malformed.as_object_mut().unwrap())
                    .is_err(),
                "L10-L13 malformed placement"
            );
        }
    }

    fn baseline_inputs(environment: EnvironmentSnapshot) -> SessionBaselineInputs {
        SessionBaselineInputs {
            environment,
            runtime_placement: SessionRuntimePlacement::Local,
            mcp_authoring: SessionMcpAuthoringContext::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model: "model".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        }
    }

    fn baseline(environment: EnvironmentSnapshot) -> SessionBaseline {
        SessionBaseline::compile(baseline_inputs(environment))
    }

    fn control_inputs(network: SessionNetworkPolicy) -> ControlSessionCreationInputs {
        ControlSessionCreationInputs {
            mutation_policy: SessionMutationPolicy::Managed,
            application_origin: None,
            environment: environment(1, network),
            runtime_placement: SessionRuntimePlacement::Local,
            agent_id: "agent".into(),
            agent_revision: None,
            inline_agent_publication: None,
            model: "model".into(),
            execution_model_ref: "model".into(),
            model_override: None,
            system_prompt: SessionSystemPromptSelection::Inherit,
            runtime: Some("native".into()),
            mcp_authoring: SessionMcpAuthoringContext::default(),
            delegate_ids: vec!["delegate".into()],
            toolsets: Vec::new(),
            mounts: vec![mount("control")],
            env: vec![env_var("CONTROL")],
            prompts: vec!["control".into()],
            transcript_prefix: None,
            resources: crate::ResolvedSessionResources::default(),
            initial_mcp: Vec::new(),
        }
    }

    #[test]
    fn inline_agent_publication_is_pinned_by_its_content_address() {
        // Causes: C1 the intent carries no inline publication; C2 it carries
        // one; C3 it carries another (different content address); C4 the
        // frozen baseline round-trips through its durable encoding.
        // Effects: E1 C1 keeps the historical fingerprint byte-for-byte and
        // carries none; E2 C2 is a different baseline carrying exactly that
        // publication; E3 C3 differs from C2; E4 C4 is lossless.
        // Rules: P1=C1=>E1, P2=C2=>E2, P3=C3=>E3, P4=C2+C4=>E4.
        let publication = |model: &str| {
            let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent")
                .model(
                    awaken_runtime_contract::resolved::ModelBinding::parse(
                        "default", model, "default",
                    )
                    .expect("a valid fixture binding"),
                )
                .build();
            snapshot.recompute_fingerprint().unwrap();
            Box::new(snapshot)
        };
        let finalize = |inline: Option<Box<awaken_runtime_contract::ExecutableAgentSnapshot>>| {
            let mut control = control_inputs(SessionNetworkPolicy::None);
            control.inline_agent_publication = inline;
            SessionCreationIntent { control }
                .finalize()
                .unwrap()
                .baseline
        };
        let historical = SessionCreationIntent {
            control: control_inputs(SessionNetworkPolicy::None),
        }
        .finalize()
        .unwrap()
        .baseline;
        let none = finalize(None);
        assert_eq!(none.fingerprint, historical.fingerprint, "P1");
        assert!(none.inline_agent_publication.is_none(), "P1");
        assert!(
            !serde_json::to_value(&none)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("inline_agent_publication"),
            "P1 the historical encoding"
        );
        let pinned = finalize(Some(publication("model-a")));
        assert_ne!(pinned.fingerprint, none.fingerprint, "P2");
        assert_eq!(
            pinned.inline_agent_publication,
            Some(publication("model-a")),
            "P2"
        );
        let other = finalize(Some(publication("model-b")));
        assert_ne!(other.fingerprint, pinned.fingerprint, "P3");
        let decoded: SessionBaseline =
            serde_json::from_value(serde_json::to_value(&pinned).unwrap()).unwrap();
        assert_eq!(decoded, pinned, "P4");
    }

    #[test]
    fn inspection_preserves_environment_but_never_exports_execution_secrets_or_mount_bytes() {
        // DT-SAFE: C1 exact frozen Environment; C2 inline declared value;
        // C3 secret env capability; C4 inline mount contents. R1 preserve C1/C2;
        // R2 mask C3 and omit C4 bytes while retaining paths/access. The read
        // projection must not serialize the complete executable baseline.
        let mut baseline =
            SessionBaseline::compile(baseline_inputs(environment(4, SessionNetworkPolicy::None)));
        baseline.env.push(EnvVar {
            name: "TZ".into(),
            value: EnvValue::Inline {
                value: "UTC".into(),
            },
            visibility: EnvVisibility::Process,
        });
        baseline.env.push(EnvVar {
            name: "API_KEY".into(),
            value: EnvValue::Secret {
                reference: "do-not-export-capability".into(),
            },
            visibility: EnvVisibility::Process,
        });
        let mut inline = mount("visible-mount");
        inline.source = MountSource::Inline {
            contents: "do-not-export-inline-bytes".into(),
        };
        baseline.mounts.push(inline);
        let view = crate::SessionConfigurationInspection::from_baseline(&baseline);
        assert_eq!(view.environment, baseline.environment);
        let wire = serde_json::to_value(view).unwrap();
        assert!(wire.to_string().contains("UTC"));
        assert!(!wire.to_string().contains("do-not-export-capability"));
        assert!(!wire.to_string().contains("do-not-export-inline-bytes"));
        assert!(
            wire["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|mount| mount.get("contents").is_none() && mount.get("reference").is_none())
        );
        assert!(wire["publication"].is_null());
    }

    #[test]
    fn public_model_id_and_execution_model_ref_are_frozen_as_distinct_facts() {
        // Causes: C1 public id may be provider/endpoint qualified; C2 the
        // publication resolves an exact upstream model coordinate. Effects: E1
        // both facts survive independently; E2 changing only the execution
        // coordinate changes the baseline fingerprint.
        //
        // | Rule | public id | execution ref | Effect |
        // | M1 | qualified | upstream-a | E1 |
        // | M2 | same | upstream-b | E2 |
        let mut inputs = baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted));
        inputs.model = "qwen/qwen3-235b;provider=anyrouter;api=open_ai_chat".into();
        let first =
            SessionBaseline::compile_with_execution_model_ref(inputs, "qwen/qwen3-235b".into());
        let mut inputs = baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted));
        inputs.model = first.model.clone();
        let second =
            SessionBaseline::compile_with_execution_model_ref(inputs, "upstream-alias".into());

        assert_eq!(
            first.model, "qwen/qwen3-235b;provider=anyrouter;api=open_ai_chat",
            "M1"
        );
        assert_eq!(first.execution_model_ref, "qwen/qwen3-235b", "M1");
        assert_ne!(first.fingerprint, second.fingerprint, "M2");
    }

    fn mcp_draft(
        name: &str,
        url: &str,
        origin: crate::McpAttachmentOrigin,
    ) -> crate::McpAttachmentDraft {
        crate::McpAttachmentDraft {
            name: name.into(),
            target: crate::McpTarget::parse_http(url).unwrap(),
            prompts_as_skills: false,
            credential: None,
            origin,
        }
    }

    #[test]
    fn creation_replay_preserves_resource_and_mcp_authority_independently() {
        // R1 a complete creation intent replays without changing frozen facts.
        // R2 only a Resource input changes -> only its manifest changes.
        // R3 only the selected MCP target changes -> only its generation changes.
        // R4 duplicate Resource identity -> durable decoding refuses the whole intent.
        let mut control = control_inputs(SessionNetworkPolicy::Unrestricted);
        control.resources = control
            .resources
            .attach(crate::resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::new("input"),
                source: crate::resource::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from("file"),
                },
                mount_path: "/inputs/file".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            })
            .unwrap();
        control.initial_mcp.extend([
            mcp_draft(
                "lookup",
                "https://agent.test/mcp",
                crate::McpAttachmentOrigin::Agent,
            ),
            mcp_draft(
                "lookup",
                "https://session.test/mcp",
                crate::McpAttachmentOrigin::Session,
            ),
        ]);
        let intent = SessionCreationIntent { control };
        let wire = serde_json::to_value(&intent).unwrap();
        let compile = |value| {
            serde_json::from_value::<SessionCreationIntent>(value)
                .unwrap()
                .finalize()
                .unwrap()
        };
        let original = intent.finalize().unwrap();
        let replayed = compile(wire.clone());
        assert_eq!(replayed, original, "R1 exact durable replay");
        assert_eq!(replayed.initial_mcp.len(), 1, "R1 Session precedence");
        let generations = |creation: &CompiledSessionCreation| {
            crate::mcp::SessionMcpAttachmentSet::from_initial(creation.initial_mcp.clone(), None)
                .unwrap()
        };
        let first = generations(&original);
        assert_eq!(
            generations(&replayed),
            first,
            "R1 exact generation identity"
        );

        let mut changed_resource = wire.clone();
        changed_resource["control"]["resources"]["inputs"][0]["mount_path"] =
            serde_json::json!("/inputs/other");
        let changed_resource = compile(changed_resource);
        assert_ne!(
            changed_resource.initial_resources, original.initial_resources,
            "R2"
        );
        assert_eq!(changed_resource.baseline, original.baseline, "R2 baseline");
        assert_eq!(generations(&changed_resource), first, "R2 MCP authority");

        let mut changed_mcp = wire.clone();
        changed_mcp["control"]["initial_mcp"][1]["target"] = serde_json::to_value(
            awaken_agent_contract::McpTarget::parse_http("https://replacement.test/mcp").unwrap(),
        )
        .unwrap();
        let changed_mcp = compile(changed_mcp);
        assert_eq!(
            changed_mcp.initial_resources, original.initial_resources,
            "R3 Resource authority"
        );
        assert_ne!(
            generations(&changed_mcp).desired_fingerprint,
            first.desired_fingerprint,
            "R3"
        );

        let mut duplicate_resource = wire;
        let input = duplicate_resource["control"]["resources"]["inputs"][0].clone();
        duplicate_resource["control"]["resources"]["inputs"]
            .as_array_mut()
            .unwrap()
            .push(input);
        assert!(
            serde_json::from_value::<SessionCreationIntent>(duplicate_resource).is_err(),
            "R4"
        );
    }

    #[test]
    fn creation_finalization_cases_follow_the_decision_table() {
        // Cause/effect graph: complete immutable creation inputs are supplied by
        // Control -> MCP drafts resolve through the one Session-over-Agent
        // precedence rule -> baseline, resources, and generation-1 MCP freeze
        // together. Duplicate selected targets fail before persistence.
        //
        // | Rule | Up-front input | Same MCP name | Selected targets | Effect |
        // |---|---|---|---|---|
        // | Z1 | complete | none | unique | freeze every supplied fact |
        // | Z2 | complete | Session + Agent | unique | Session draft wins |
        // | Z3 | complete | none | duplicate | MCP conflict |
        let mut z1_control = control_inputs(SessionNetworkPolicy::Unrestricted);
        z1_control.resources = z1_control
            .resources
            .attach(crate::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::new("input"),
                source: crate::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from("file"),
                },
                mount_path: "/inputs/file".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            })
            .unwrap();
        let z1 = SessionCreationIntent {
            control: z1_control,
        }
        .finalize()
        .expect("Z1");
        assert_eq!(z1.baseline.mounts.len(), 1, "Z1");
        assert_eq!(z1.baseline.env.len(), 1, "Z1");
        assert_eq!(z1.baseline.prompts, vec!["control"], "Z1");
        assert_eq!(z1.initial_resources.inputs().len(), 1, "Z1");

        let mut z2_control = control_inputs(SessionNetworkPolicy::Unrestricted);
        z2_control.initial_mcp.extend([
            mcp_draft(
                "calc",
                "https://agent.example",
                crate::McpAttachmentOrigin::Agent,
            ),
            mcp_draft(
                "calc",
                "https://session.example",
                crate::McpAttachmentOrigin::Session,
            ),
        ]);
        let z2 = SessionCreationIntent {
            control: z2_control,
        }
        .finalize()
        .expect("Z2");
        assert_eq!(z2.initial_mcp.len(), 1, "Z2");
        assert_eq!(
            z2.initial_mcp[0].target.http_url(),
            Some("https://session.example"),
            "Z2"
        );

        let mut z3_control = control_inputs(SessionNetworkPolicy::Unrestricted);
        z3_control.initial_mcp.extend([
            mcp_draft(
                "first",
                "https://same.example",
                crate::McpAttachmentOrigin::Agent,
            ),
            mcp_draft(
                "second",
                "https://same.example",
                crate::McpAttachmentOrigin::Session,
            ),
        ]);
        assert!(
            matches!(
                (SessionCreationIntent {
                    control: z3_control
                })
                .finalize(),
                Err(SessionCreationFinalizeError::McpConflict(_))
            ),
            "Z3"
        );
    }
    #[test]
    fn baseline_fingerprint_decision_table() {
        // Cause graph: equal normalized facts -> equal fingerprint; changing the
        // Environment revision, normalized network fact, or mount projection ->
        // different fingerprint.
        //
        // | Rule | Same revision | Same network | Effect |
        // |------|---------------|--------------|--------|
        // | B1   | T             | T            | equal  |
        // | B2   | F             | T            | differ |
        // | B3   | T             | F            | differ |
        // | B4   | T             | T + mount | differ |
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
        let mut with_mount = baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted));
        with_mount.mounts = vec![mount("workspace")];
        assert_ne!(
            original.fingerprint,
            SessionBaseline::compile(with_mount).fingerprint,
            "B4"
        );
    }

    #[test]
    fn mutation_policy_preserves_legacy_shape_and_fences_profiled_fingerprints() {
        // Cause/effect graph: C1 a historical baseline omits mutation policy;
        // C2 an ordinary Managed baseline selects the default; C3/C4 the same
        // immutable facts select Frozen/FileResources. Effects: E1 legacy
        // decode is Managed, E2 Managed retains the pre-policy fingerprint and
        // omitted wire shape, E3 each profiled policy has a distinct immutable
        // fingerprint. Constraint: policy is selected only at finalization.
        //
        // | Rule | Policy field | Selected policy | Effect |
        // | P1 | absent | Managed | E1 + E2 |
        // | P2 | Managed | Managed | E2 |
        // | P3 | Frozen | Frozen | E3 |
        // | P4 | FileResources | FileResources | E3 |
        let inputs = || baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted));
        let managed = SessionBaseline::compile(inputs());
        let frozen = SessionBaseline::compile_with_policy_and_execution_model_ref(
            inputs(),
            "model".into(),
            SessionMutationPolicy::Frozen,
        );
        let files = SessionBaseline::compile_with_policy_and_execution_model_ref(
            inputs(),
            "model".into(),
            SessionMutationPolicy::FileResources,
        );

        let mut legacy = serde_json::to_value(&managed).expect("P1 encode");
        assert!(legacy.get("mutation_policy").is_none(), "P2/E2");
        legacy
            .as_object_mut()
            .expect("baseline object")
            .remove("mutation_policy");
        let decoded: SessionBaseline = serde_json::from_value(legacy).expect("P1 decode");
        assert_eq!(
            decoded.mutation_policy,
            SessionMutationPolicy::Managed,
            "P1/E1"
        );
        assert_eq!(decoded.fingerprint, managed.fingerprint, "P1/E2");
        assert_ne!(frozen.fingerprint, managed.fingerprint, "P3/E3");
        assert_ne!(files.fingerprint, managed.fingerprint, "P4/E3");
        assert_ne!(files.fingerprint, frozen.fingerprint, "P3/P4/E3");
    }

    #[test]
    fn system_prompt_selection_preserves_legacy_inheritance_and_clear_replace_semantics() {
        // Cause/effect graph: C1 historical creation/baseline omits the system
        // selection; C2 inherit is explicit; C3 clear is selected; C4 a value is
        // selected. Effects: E1 C1/C2 inherit the exact Agent publication and
        // retain the legacy serialized shape/fingerprint; E2 C3 projects no
        // system text; E3 C4 projects the replacement; E4 C3/C4 are distinct
        // immutable baseline facts. Decision rules S1=C1/C2=>E1,
        // S2=C3=>E2+E4, S3=C4=>E3+E4.
        let inherited = SessionCreationIntent {
            control: control_inputs(SessionNetworkPolicy::Unrestricted),
        }
        .finalize()
        .expect("S1 finalize")
        .baseline;
        let mut clear = control_inputs(SessionNetworkPolicy::Unrestricted);
        clear.system_prompt = SessionSystemPromptSelection::Clear;
        let clear = SessionCreationIntent { control: clear }
            .finalize()
            .expect("S2 finalize")
            .baseline;
        let mut replaced = control_inputs(SessionNetworkPolicy::Unrestricted);
        replaced.system_prompt = SessionSystemPromptSelection::Replace("session rules".into());
        let replaced = SessionCreationIntent { control: replaced }
            .finalize()
            .expect("S3 finalize")
            .baseline;

        assert!(
            serde_json::to_value(&inherited)
                .expect("S1 encode")
                .get("system_prompt")
                .is_none(),
            "S1/E1"
        );
        assert_eq!(
            inherited
                .system_prompt
                .resolve(Some("published rules".into()))
                .as_deref(),
            Some("published rules"),
            "S1/E1"
        );
        assert_eq!(
            clear.system_prompt.resolve(Some("published rules".into())),
            None,
            "S2/E2"
        );
        assert_eq!(
            replaced
                .system_prompt
                .resolve(Some("published rules".into())),
            Some("session rules".into()),
            "S3/E3"
        );
        assert_ne!(clear.fingerprint, inherited.fingerprint, "S2/E4");
        assert_ne!(replaced.fingerprint, inherited.fingerprint, "S3/E4");
        assert_ne!(replaced.fingerprint, clear.fingerprint, "S2/S3/E4");
    }

    #[test]
    fn application_origin_is_frozen_without_changing_legacy_or_model_context() {
        // Causes: C1 origin absent/present; C2 Managed/Frozen/FileResources;
        // C3 one source, context fingerprint or evidence changes; C4 durable
        // intent/baseline round-trip. Effects: E1 absence retains historical
        // bytes/fingerprint; E2 complete origin survives finalization; E3 each
        // changed binding changes fingerprint; E4 no evidence enters prompts.
        // R1 !C1+any C2 => E1. R2 C1+any C2+C4 => E2+E4. R3 R2+C3 => E3.
        // Source/evidence shape refusal is owned by the carrier's decision table.
        let origin = |source: &str, digest: char, proof: &str| {
            crate::ApplicationOriginBinding::new(
                source.into(),
                digest.to_string().repeat(64),
                proof.into(),
            )
            .unwrap()
        };
        for policy in [
            SessionMutationPolicy::Managed,
            SessionMutationPolicy::Frozen,
            SessionMutationPolicy::FileResources,
        ] {
            let mut old_inputs = control_inputs(SessionNetworkPolicy::Unrestricted);
            old_inputs.mutation_policy = policy;
            let old_intent = SessionCreationIntent {
                control: old_inputs,
            };
            let old_wire = serde_json::to_value(&old_intent).unwrap();
            assert!(
                old_wire["control"].get("application_origin").is_none(),
                "R1 intent"
            );
            let old = old_intent.clone().finalize().unwrap().baseline;
            let old_baseline_wire = serde_json::to_value(&old).unwrap();
            assert!(
                old_baseline_wire.get("application_origin").is_none(),
                "R1 baseline"
            );
            let legacy: SessionCreationIntent = serde_json::from_value(old_wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&legacy).unwrap(), old_wire, "R1 bytes");
            assert_eq!(
                legacy.finalize().unwrap().baseline.fingerprint,
                old.fingerprint,
                "R1 fingerprint"
            );
            let old_reload: SessionBaseline = serde_json::from_value(old_baseline_wire).unwrap();
            assert_eq!(old_reload, old, "R1 legacy baseline");
            let original = origin("application.interaction:v1", 'a', "original-proof");
            let mut with_origin = old_intent.clone();
            with_origin.control.application_origin = Some(original.clone());
            let wire = serde_json::to_value(&with_origin).unwrap();
            let accepted: SessionCreationIntent = serde_json::from_value(wire).unwrap();
            let frozen = accepted.finalize().unwrap().baseline;
            assert_eq!(frozen.application_origin, Some(original), "R2");
            assert_eq!(frozen.prompts, old.prompts, "R2 model context");
            assert!(
                !frozen
                    .prompts
                    .iter()
                    .any(|prompt| prompt.contains("original-proof")),
                "R2 evidence excluded"
            );
            assert_ne!(frozen.fingerprint, old.fingerprint, "R2 fingerprint");
            assert_eq!(
                serde_json::from_value::<SessionBaseline>(serde_json::to_value(&frozen).unwrap())
                    .unwrap(),
                frozen,
                "R2 durable round-trip"
            );
            for changed in [
                origin("application.other:v1", 'a', "original-proof"),
                origin("application.interaction:v1", 'b', "original-proof"),
                origin("application.interaction:v1", 'a', "changed-proof"),
            ] {
                let mut next = old_intent.clone();
                next.control.application_origin = Some(changed);
                assert_ne!(
                    next.finalize().unwrap().baseline.fingerprint,
                    frozen.fingerprint,
                    "R3 complete binding fence"
                );
            }
        }
    }

    #[test]
    fn baseline_requires_complete_canonical_shape() {
        // Grammar partition: B1/B2 canonical local/worker shapes round-trip;
        // B3 missing placement and B4 unknown field are rejected. This prevents
        // topology-dependent recovery from changing a frozen ownership fact.
        for (rule, placement) in [
            ("B1", SessionRuntimePlacement::Local),
            ("B2", SessionRuntimePlacement::Worker),
        ] {
            let baseline = SessionBaseline::compile(SessionBaselineInputs {
                runtime_placement: placement,
                ..baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted))
            });
            let mut encoded = serde_json::to_value(&baseline).expect("encode explicit placement");
            let decoded: SessionBaseline =
                serde_json::from_value(encoded.clone()).expect("decode explicit placement");
            assert_eq!(decoded.runtime_placement, placement, "{rule}");
            encoded["unknown"] = serde_json::json!(true);
            assert!(
                serde_json::from_value::<SessionBaseline>(encoded).is_err(),
                "B4"
            );
        }

        let mut missing = serde_json::to_value(SessionBaseline::compile(baseline_inputs(
            environment(1, SessionNetworkPolicy::Unrestricted),
        )))
        .unwrap();
        missing.as_object_mut().unwrap().remove("runtime_placement");
        assert!(
            serde_json::from_value::<SessionBaseline>(missing).is_err(),
            "B3"
        );
    }
}
