//! Application-owned, claim-bound additions to one Session environment.
//!
//! The host remains the sole owner of Session realization. An embedding
//! application may prepare mounts, environment values, prompt context, and MCP
//! servers after a dispatch is claimed, but the result is staged into the same
//! Session slot and realized by the same Native/ACP backend path.

use std::collections::HashSet;
use std::sync::Arc;

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
    pub deny_egress: bool,
}

impl ApplicationSessionPlan {
    #[must_use]
    pub fn empty(fingerprint: impl Into<String>) -> Self {
        Self {
            fingerprint: fingerprint.into(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            deny_egress: false,
        }
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

impl crate::SharedHost {
    pub(crate) fn install_application_session_plan(
        &self,
        thread: &str,
        plan: ApplicationSessionPlan,
    ) -> Result<(), crate::HostError> {
        if plan.fingerprint.trim().is_empty() {
            return Err(crate::HostError::internal(
                "application Session plan fingerprint must not be empty",
            ));
        }

        if let Some(existing) = self
            .session_slots
            .read(thread, |slot| slot.application.clone())
            .flatten()
        {
            return if existing.fingerprint == plan.fingerprint {
                Ok(())
            } else {
                Err(crate::HostError::internal(format!(
                    "thread {thread} is already bound to a different application Session plan"
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
                "thread {thread} was realized before its application Session plan"
            )));
        }

        validate_plan(&plan, &built_in_mounts)?;
        self.session_slots
            .update(thread, |slot| slot.application = Some(plan));
        Ok(())
    }

    pub(crate) fn thread_session_mounts(
        &self,
        thread: &str,
    ) -> Vec<awaken_provisioning_contract::MountRequirement> {
        self.session_slots
            .read(thread, |slot| {
                let mut mounts = slot.resources.mounts.clone();
                if let Some(application) = &slot.application {
                    mounts.extend(application.mounts.clone());
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
                slot.application
                    .as_ref()
                    .map(|application| application.env.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    pub(crate) fn thread_session_prompts(&self, thread: &str) -> Vec<String> {
        self.session_slots
            .read(thread, |slot| {
                let mut prompts = slot.resources.prompts.clone();
                if let Some(application) = &slot.application {
                    prompts.extend(application.prompts.clone());
                }
                prompts
            })
            .unwrap_or_default()
    }
}

fn validate_plan(
    plan: &ApplicationSessionPlan,
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
    for mount in &plan.mounts {
        if mount.mount_id.trim().is_empty()
            || mount.mount_path.trim().is_empty()
            || !mount_ids.insert(&mount.mount_id)
            || !mount_paths.insert(&mount.mount_path)
        {
            return Err(crate::HostError::internal(
                "application Session plan has an empty or conflicting mount",
            ));
        }
    }

    let mut env_names = HashSet::new();
    for env in &plan.env {
        if env.name.trim().is_empty() || !env_names.insert(env.name.as_str()) {
            return Err(crate::HostError::internal(
                "application Session plan has an empty or duplicate environment variable",
            ));
        }
    }

    Ok(())
}
