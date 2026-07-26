//! Application-owned, claim-bound additions to one Session environment.
//!
//! The host remains the sole owner of Session realization. An embedding
//! application may prepare mounts, environment values, prompt context, and MCP
//! servers after a dispatch is claimed, but the result is staged into the same
//! Session slot and realized by the same Native/ACP backend path.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_run_ingress::{RunClaim, WorkerIdentity};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::runtime_context::AttemptOwnershipVerifier;

/// Frozen application additions for one claimed Session.
///
/// `fingerprint` is the application's stable identity for the complete plan.
/// Re-delivery of the same plan is idempotent; a different plan cannot mutate an
/// already-bound Session and fails closed.
#[derive(Clone)]
pub struct ApplicationSessionPlan {
    pub fingerprint: String,
    pub mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub env: Vec<awaken_provisioning_contract::EnvVar>,
    pub prompts: Vec<String>,
    pub mcp_inputs: Vec<serde_json::Value>,
    pub network_restriction: Option<awaken_protocol_managed::SessionNetworkPolicy>,
}

impl ApplicationSessionPlan {
    #[must_use]
    pub fn empty(fingerprint: impl Into<String>) -> Self {
        Self {
            fingerprint: fingerprint.into(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            mcp_inputs: Vec::new(),
            network_restriction: None,
        }
    }

    fn into_contribution(
        self,
        session_id: String,
    ) -> Result<awaken_protocol_managed::ApplicationSessionContribution, ApplicationSessionError>
    {
        let mounts = self
            .mounts
            .into_iter()
            .map(|mount| serde_json::to_value(mount).map_err(ApplicationSessionError::from_error))
            .collect::<Result<Vec<_>, _>>()?;
        let env = self
            .env
            .into_iter()
            .map(|value| serde_json::to_value(value).map_err(ApplicationSessionError::from_error))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(awaken_protocol_managed::ApplicationSessionContribution {
            session_id,
            application_fingerprint: self.fingerprint,
            input: awaken_protocol_managed::ApplicationSessionInput {
                mounts,
                env,
                prompts: self.prompts,
                mcp_inputs: self.mcp_inputs,
                network_restriction: self.network_restriction,
            },
        })
    }
}

/// Failure while an application projects a claimed Run into Session additions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationSessionError(String);

impl ApplicationSessionError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn from_error(error: impl std::fmt::Display) -> Self {
        Self(error.to_string())
    }
}

impl std::fmt::Display for ApplicationSessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ApplicationSessionError {}

/// Claim-time application projection port.
///
/// The ownership verifier is neutral runtime wiring. Implementations should
/// check it around credential materialization or any other external operation;
/// the host also checks before and after the complete projection.
#[async_trait::async_trait]
pub trait ApplicationSessionProvisioner: Send + Sync {
    async fn prepare(
        &self,
        activation: &RunActivation,
        ownership: Arc<dyn AttemptOwnershipVerifier>,
    ) -> Result<ApplicationSessionPlan, ApplicationSessionError>;
}

/// Result of the claim-fenced contribution and initial realization assignment.
#[derive(Clone)]
pub struct ApplicationSessionControlReceipt {
    pub contribution: awaken_protocol_managed::ApplicationSessionContributionReceipt,
    pub realization: awaken_protocol_managed::SessionRealizationDirective,
}

/// Worker-side outbound port to the authenticated Control-owned Session
/// application service. Contribution and realization phases cannot be wired to
/// different authorities.
#[async_trait::async_trait]
pub trait ApplicationSessionControlClient: Send + Sync {
    async fn contribute(
        &self,
        session_id: &str,
        claim: &RunClaim,
        plan: ApplicationSessionPlan,
    ) -> Result<ApplicationSessionControlReceipt, ApplicationSessionError>;

    async fn activate(
        &self,
        command: awaken_protocol_managed::ActivateSessionRealization,
    ) -> Result<awaken_protocol_managed::SessionRealizationDirective, ApplicationSessionError>;

    async fn acknowledge(
        &self,
        command: awaken_protocol_managed::AcknowledgeSessionRealization,
    ) -> Result<awaken_protocol_managed::SessionRealizationDirective, ApplicationSessionError>;

    async fn fail(
        &self,
        command: awaken_protocol_managed::FailSessionRealization,
    ) -> Result<(), ApplicationSessionError>;
}

/// Standard client using the same registered identity-bound Worker transport as
/// lifecycle, dispatch, recovery, and claimed commits.
pub struct WorkerControlApplicationSessionClient {
    control: crate::WorkerControlClient,
    identity: WorkerIdentity,
}

impl WorkerControlApplicationSessionClient {
    #[must_use]
    pub fn new(control: crate::WorkerControlClient, identity: WorkerIdentity) -> Self {
        Self { control, identity }
    }
}

#[async_trait::async_trait]
impl ApplicationSessionControlClient for WorkerControlApplicationSessionClient {
    async fn contribute(
        &self,
        session_id: &str,
        claim: &RunClaim,
        plan: ApplicationSessionPlan,
    ) -> Result<ApplicationSessionControlReceipt, ApplicationSessionError> {
        let contribution = plan.into_contribution(session_id.to_string())?;
        self.control
            .contribute_application(&self.identity, claim, contribution)
            .await
            .map_err(ApplicationSessionError::new)
    }

    async fn activate(
        &self,
        command: awaken_protocol_managed::ActivateSessionRealization,
    ) -> Result<awaken_protocol_managed::SessionRealizationDirective, ApplicationSessionError> {
        self.control
            .activate_session_realization(&self.identity, command)
            .await
            .map_err(ApplicationSessionError::new)
    }

    async fn acknowledge(
        &self,
        command: awaken_protocol_managed::AcknowledgeSessionRealization,
    ) -> Result<awaken_protocol_managed::SessionRealizationDirective, ApplicationSessionError> {
        self.control
            .acknowledge_session_realization(&self.identity, command)
            .await
            .map_err(ApplicationSessionError::new)
    }

    async fn fail(
        &self,
        command: awaken_protocol_managed::FailSessionRealization,
    ) -> Result<(), ApplicationSessionError> {
        self.control
            .fail_session_realization(&self.identity, command)
            .await
            .map_err(ApplicationSessionError::new)
    }
}

impl crate::SharedHost {
    pub(crate) async fn install_frozen_session_projection(
        &self,
        thread: &str,
        projection: awaken_protocol_managed::FrozenSessionProjection,
    ) -> Result<(), crate::HostError> {
        if projection.baseline.fingerprint.0.trim().is_empty() {
            return Err(crate::HostError::internal(
                "frozen Session baseline fingerprint must not be empty",
            ));
        }
        if projection.baseline.application.is_none() {
            return Err(crate::HostError::internal(
                "application contribution returned a baseline without its durable receipt",
            ));
        }
        if !projection.mcp.is_empty() {
            return Err(crate::HostError::internal(
                "remote initial MCP realization is not installed",
            ));
        }
        let baseline = decode_baseline_projection(&projection.baseline)?;

        if let Some(existing) = self
            .session_slots
            .read(thread, |slot| slot.baseline.clone())
            .flatten()
        {
            return if existing.fingerprint == baseline.fingerprint {
                Ok(())
            } else {
                Err(crate::HostError::internal(format!(
                    "thread {thread} is already bound to a different frozen Session baseline"
                )))
            };
        }

        let occupied = self.session_slots.read(thread, |slot| {
            (
                slot.runtime.is_some() || slot.environment.is_some(),
                slot.resources.mounts.clone(),
            )
        });
        let (is_realized, built_in_mounts) = occupied.unwrap_or_else(|| (false, Vec::new()));
        if is_realized {
            return Err(crate::HostError::internal(format!(
                "thread {thread} was realized before its frozen Session baseline"
            )));
        }

        validate_baseline_projection(&baseline, &built_in_mounts)?;
        if projection.resources != awaken_protocol_managed::ResolvedSessionResources::default() {
            let manifest = awaken_protocol_managed::SessionResourceManifest::new(
                projection.workspace_id.clone(),
                projection.resources,
            );
            self.install_dispatched_resources(thread, &manifest)
                .await
                .map_err(|error| crate::HostError::internal(error.to_string()))?;
        }
        self.session_slots
            .update(thread, |slot| slot.baseline = Some(baseline));
        self.register_thread_workspace(thread, &projection.workspace_id);
        self.register_thread_model(thread, &projection.baseline.model);
        if let Some(runtime) = &projection.baseline.runtime {
            self.register_thread_runtime(thread, runtime);
        }
        self.register_thread_delegates(thread, projection.baseline.delegate_ids);
        self.register_thread_credential_realization(
            thread,
            projection
                .baseline
                .environment
                .credential_realization
                .clone(),
        );
        self.register_thread_egress(
            thread,
            projection.baseline.environment.network.is_restricted(),
        );
        if let Some(sandbox) = awaken_provisioning_contract::SandboxOverride::from_config_value(
            &projection.baseline.environment.sandbox,
        ) {
            self.register_thread_sandbox(thread, sandbox);
        }
        Ok(())
    }

    pub(crate) fn thread_session_mounts(
        &self,
        thread: &str,
    ) -> Vec<awaken_provisioning_contract::MountRequirement> {
        self.session_slots
            .read(thread, |slot| {
                let mut mounts = slot.resources.mounts.clone();
                if let Some(baseline) = &slot.baseline {
                    mounts.extend(baseline.mounts.clone());
                }
                mounts
            })
            .unwrap_or_default()
    }

    pub(crate) fn thread_session_env(
        &self,
        thread: &str,
    ) -> Vec<awaken_provisioning_contract::EnvVar> {
        self.session_slots
            .read(thread, |slot| {
                slot.baseline
                    .as_ref()
                    .map(|baseline| baseline.env.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    pub(crate) fn thread_session_prompts(&self, thread: &str) -> Vec<String> {
        self.session_slots
            .read(thread, |slot| {
                let mut prompts = slot.resources.prompts.clone();
                if let Some(baseline) = &slot.baseline {
                    prompts.extend(baseline.prompts.clone());
                }
                prompts
            })
            .unwrap_or_default()
    }
}

fn decode_baseline_projection(
    baseline: &awaken_protocol_managed::SessionBaseline,
) -> Result<crate::session_slot::FrozenBaselineRuntimeProjection, crate::HostError> {
    let mounts = baseline
        .mounts
        .iter()
        .cloned()
        .map(|value| {
            serde_json::from_value(value).map_err(|error| {
                crate::HostError::internal(format!(
                    "frozen Session baseline has an invalid mount: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let env = baseline
        .env
        .iter()
        .cloned()
        .map(|value| {
            serde_json::from_value(value).map_err(|error| {
                crate::HostError::internal(format!(
                    "frozen Session baseline has an invalid environment value: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let network = match &baseline.environment.network {
        awaken_protocol_managed::SessionNetworkPolicy::Unrestricted => {
            awaken_provisioning_contract::NetworkPolicy::Unrestricted
        }
        awaken_protocol_managed::SessionNetworkPolicy::Allowlist { hosts } => {
            awaken_provisioning_contract::NetworkPolicy::Allowlist {
                hosts: hosts.clone(),
            }
        }
        awaken_protocol_managed::SessionNetworkPolicy::None => {
            awaken_provisioning_contract::NetworkPolicy::None
        }
    };
    Ok(crate::session_slot::FrozenBaselineRuntimeProjection {
        fingerprint: baseline.fingerprint.clone(),
        mounts,
        env,
        prompts: baseline.prompts.clone(),
        network,
    })
}

fn validate_baseline_projection(
    baseline: &crate::session_slot::FrozenBaselineRuntimeProjection,
    built_in_mounts: &[awaken_provisioning_contract::MountRequirement],
) -> Result<(), crate::HostError> {
    let mut mount_ids: HashSet<&str> = built_in_mounts
        .iter()
        .map(|mount| mount.mount_id.as_str())
        .collect();
    let mut mount_paths: HashSet<&str> = built_in_mounts
        .iter()
        .map(|mount| mount.mount_path.as_str())
        .collect();
    for mount in &baseline.mounts {
        if mount.mount_id.trim().is_empty()
            || mount.mount_path.trim().is_empty()
            || !mount_ids.insert(&mount.mount_id)
            || !mount_paths.insert(&mount.mount_path)
        {
            return Err(crate::HostError::internal(
                "frozen Session baseline has an empty or conflicting mount",
            ));
        }
    }

    let mut env_names = HashSet::new();
    for env in &baseline.env {
        if env.name.trim().is_empty() || !env_names.insert(env.name.as_str()) {
            return Err(crate::HostError::internal(
                "frozen Session baseline has an empty or duplicate environment variable",
            ));
        }
    }

    Ok(())
}
