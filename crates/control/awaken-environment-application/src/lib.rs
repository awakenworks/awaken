//! Canonical Control application service for static Environment definitions.
//!
//! The service is the only command path that coordinates `EnvRegistry`, exact
//! sandbox-policy versions, and executable registration. HTTP, Admin Assistant,
//! standalone Control, and AllInOne all reuse this implementation.

use std::sync::Arc;

use awaken_admin_assistant::{AdminEnvironmentNetworking, EnvironmentAuthor, EnvironmentDraft};
use awaken_environment_contract::{
    CreateEnvironmentCommand, CreateEnvironmentError, EnvItem, EnvRegistry, EnvUpdate,
    EnvironmentConfig, EnvironmentRevision, EnvironmentSandboxPolicyRef,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentWithdrawal,
};
use awaken_provisioning_contract::{
    SandboxExecutionPolicyRef, SandboxExecutionPolicyStore, SandboxExecutionPolicyVersion,
};

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentApplicationError {
    #[error(transparent)]
    Create(#[from] CreateEnvironmentError),
    #[error(transparent)]
    Registration(#[from] ExecutableEnvironmentRegistrationError),
    #[error("Environment was not found")]
    NotFound,
    #[error("Built-in Environment definitions are immutable")]
    BuiltinImmutable,
    #[error("Sandbox execution policy failed: {0}")]
    Policy(String),
}

pub struct EnvironmentApplication {
    envs: Arc<dyn EnvRegistry>,
    registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
}

impl EnvironmentApplication {
    #[must_use]
    pub fn new(
        envs: Arc<dyn EnvRegistry>,
        registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
        sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
    ) -> Self {
        Self {
            envs,
            registrar,
            sandbox_policies,
        }
    }

    #[must_use]
    pub fn with_sandbox_policies(
        &self,
        sandbox_policies: Arc<dyn SandboxExecutionPolicyStore>,
    ) -> Self {
        Self::new(
            self.envs.clone(),
            self.registrar.clone(),
            Some(sandbox_policies),
        )
    }

    async fn registration(
        &self,
        item: EnvItem,
    ) -> Result<ExecutableEnvironmentRegistration, EnvironmentApplicationError> {
        let sandbox_policy = match (&self.sandbox_policies, &item.sandbox_policy) {
            (Some(store), Some(reference)) => Some(
                store
                    .get_exact(&SandboxExecutionPolicyRef {
                        id: awaken_provisioning_contract::SandboxExecutionPolicyId(
                            reference.policy_id.clone(),
                        ),
                        version: SandboxExecutionPolicyVersion(reference.version),
                    })
                    .await
                    .map_err(|error| EnvironmentApplicationError::Policy(error.to_string()))?,
            ),
            (None, Some(_)) => {
                return Err(EnvironmentApplicationError::Policy(
                    "policy store is unavailable".into(),
                ));
            }
            (_, None) => None,
        };
        Ok(ExecutableEnvironmentRegistration::new(item, sandbox_policy))
    }

    async fn publish(&self, item: EnvItem) -> Result<EnvItem, EnvironmentApplicationError> {
        self.registrar
            .register(self.registration(item.clone()).await?)
            .await?;
        Ok(item)
    }

    pub async fn get(&self, environment_id: &str) -> Option<EnvItem> {
        if environment_id == "env_local" {
            return Some(builtin_local_environment());
        }
        self.envs.get(environment_id).await
    }

    pub async fn list_active(&self) -> Vec<EnvItem> {
        let mut items = self.envs.list_active().await;
        items.push(builtin_local_environment());
        items.sort_by(|left, right| left.id.cmp(&right.id));
        items
    }

    pub async fn create(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        let outcome = self.envs.create_once(command).await?;
        self.publish(outcome.item().clone()).await
    }

    /// Replay every immutable Control revision into the rebuildable Coordinator
    /// projection. This repairs a boundary failure after the authority commit.
    pub async fn reconcile_registrations(&self) -> Result<u64, EnvironmentApplicationError> {
        self.registrar
            .register(default_environment_registration())
            .await?;
        let mut reconciled = 1_u64;
        for current in self.envs.list_all().await {
            for revision in 1..current.revision.0 {
                if let Some(item) = self
                    .envs
                    .get_revision(&current.id, EnvironmentRevision(revision))
                    .await
                    && item.archived_at.is_none()
                {
                    self.registrar
                        .register(self.registration(item).await?)
                        .await?;
                    reconciled += 1;
                }
            }
            if current.archived_at.is_some() {
                self.registrar
                    .withdraw(ExecutableEnvironmentWithdrawal {
                        environment_id: current.id,
                        lifecycle_revision: current.revision,
                    })
                    .await?;
            } else {
                self.registrar
                    .register(self.registration(current).await?)
                    .await?;
            }
            reconciled += 1;
        }
        Ok(reconciled)
    }

    pub async fn update(
        &self,
        environment_id: &str,
        patch: EnvUpdate,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        self.ensure_mutable(environment_id)?;
        let item = self
            .envs
            .update(environment_id, patch)
            .await
            .ok_or(EnvironmentApplicationError::NotFound)?;
        self.publish(item).await
    }

    pub async fn archive(
        &self,
        environment_id: &str,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        self.ensure_mutable(environment_id)?;
        let item = self
            .envs
            .archive(environment_id)
            .await
            .ok_or(EnvironmentApplicationError::NotFound)?;
        self.registrar
            .withdraw(ExecutableEnvironmentWithdrawal {
                environment_id: item.id.clone(),
                lifecycle_revision: item.revision,
            })
            .await?;
        Ok(item)
    }

    pub async fn delete(&self, environment_id: &str) -> Result<(), EnvironmentApplicationError> {
        self.archive(environment_id).await?;
        Ok(())
    }

    pub async fn bind_sandbox_policy(
        &self,
        environment_id: &str,
        reference: SandboxExecutionPolicyRef,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        self.ensure_mutable(environment_id)?;
        let store = self.sandbox_policies.as_ref().ok_or_else(|| {
            EnvironmentApplicationError::Policy("policy store is unavailable".into())
        })?;
        let policy = store
            .get_exact(&reference)
            .await
            .map_err(|error| EnvironmentApplicationError::Policy(error.to_string()))?;
        if policy.disabled {
            return Err(EnvironmentApplicationError::Policy(
                "sandbox execution policy is disabled".into(),
            ));
        }
        if !self.envs.exists(environment_id).await {
            return Err(EnvironmentApplicationError::NotFound);
        }
        let item = self
            .envs
            .update(
                environment_id,
                EnvUpdate {
                    sandbox_policy: Some(Some(EnvironmentSandboxPolicyRef {
                        policy_id: reference.id.0,
                        version: reference.version.0,
                    })),
                    ..Default::default()
                },
            )
            .await
            .ok_or(EnvironmentApplicationError::NotFound)?;
        self.publish(item).await
    }

    fn ensure_mutable(&self, environment_id: &str) -> Result<(), EnvironmentApplicationError> {
        if environment_id == "env_local" {
            Err(EnvironmentApplicationError::BuiltinImmutable)
        } else {
            Ok(())
        }
    }
}

#[must_use]
pub fn default_environment_registration() -> ExecutableEnvironmentRegistration {
    ExecutableEnvironmentRegistration::new(builtin_local_environment(), None)
}

#[must_use]
pub fn builtin_local_environment() -> EnvItem {
    EnvItem {
        id: "env_local".into(),
        revision: EnvironmentRevision(1),
        name: "Local".into(),
        description: "Built-in local execution Environment".into(),
        metadata: Default::default(),
        scope: None,
        config: EnvironmentConfig::SelfHosted,
        sandbox_policy: None,
        archived_at: None,
    }
}

/// Admin Assistant adapter over the same Control application command path used
/// by HTTP. It performs only draft-to-domain translation.
pub struct EnvironmentApplicationAuthor {
    application: Arc<EnvironmentApplication>,
}

impl EnvironmentApplicationAuthor {
    #[must_use]
    pub fn new(application: Arc<EnvironmentApplication>) -> Self {
        Self { application }
    }
}

#[async_trait::async_trait]
impl EnvironmentAuthor for EnvironmentApplicationAuthor {
    async fn create(
        &self,
        command_id: &str,
        name: &str,
        config: EnvironmentDraft,
    ) -> Result<String, String> {
        self.application
            .create(CreateEnvironmentCommand {
                command_id: format!("control:{command_id}"),
                name: name.to_owned(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: canonical_admin_config(config),
            })
            .await
            .map(|item| item.id)
            .map_err(|error| error.to_string())
    }
}

fn canonical_admin_config(draft: EnvironmentDraft) -> EnvironmentConfig {
    match draft {
        EnvironmentDraft::SelfHosted => EnvironmentConfig::SelfHosted,
        EnvironmentDraft::Cloud {
            networking,
            packages,
        } => EnvironmentConfig::Cloud {
            networking: match networking {
                AdminEnvironmentNetworking::Unrestricted => {
                    awaken_environment_contract::EnvironmentNetworking::Unrestricted
                }
                AdminEnvironmentNetworking::Limited {
                    allowed_hosts,
                    allow_mcp_servers,
                    allow_package_managers,
                } => awaken_environment_contract::EnvironmentNetworking::Limited {
                    allowed_hosts,
                    allow_mcp_servers,
                    allow_package_managers,
                },
            },
            packages: awaken_environment_contract::EnvironmentPackages {
                kind: awaken_environment_contract::EnvironmentPackagesKind::Packages,
                apt: packages.apt,
                cargo: packages.cargo,
                gem: packages.gem,
                go: packages.go,
                npm: packages.npm,
                pip: packages.pip,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use awaken_environment_contract::{CreateEnvironmentCommand, EnvironmentConfig};
    use awaken_executable_environment_contract::{
        ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentWithdrawalOutcome,
    };

    use super::*;

    #[derive(Default)]
    struct RecordingRegistrar {
        registrations: Mutex<Vec<ExecutableEnvironmentRegistration>>,
        withdrawals: Mutex<Vec<ExecutableEnvironmentWithdrawal>>,
    }

    #[async_trait]
    impl ExecutableEnvironmentRegistrar for RecordingRegistrar {
        async fn register(
            &self,
            registration: ExecutableEnvironmentRegistration,
        ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
        {
            self.registrations.lock().unwrap().push(registration);
            Ok(ExecutableEnvironmentRegistrationOutcome::RegisteredCurrent)
        }

        async fn withdraw(
            &self,
            withdrawal: ExecutableEnvironmentWithdrawal,
        ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
        {
            self.withdrawals.lock().unwrap().push(withdrawal);
            Ok(ExecutableEnvironmentWithdrawalOutcome::WithdrawnCurrent)
        }
    }

    #[tokio::test]
    async fn static_history_and_executable_projection_have_one_command_path() {
        // Cause/effect decision table:
        // R1 valid create -> Control revision 1 + one exact registration;
        // R2 update -> append revision 2 + register it without overwriting v1;
        // R3 delete -> append terminal revision 3 + one withdrawal, retain history;
        // R4 built-in mutation -> reject without store or projection effect.
        let envs: Arc<dyn EnvRegistry> = Arc::new(awaken_env_store::InMemoryEnvRegistry::new());
        let registrar = Arc::new(RecordingRegistrar::default());
        let application = EnvironmentApplication::new(envs.clone(), registrar.clone(), None);
        let created = application
            .create(CreateEnvironmentCommand {
                command_id: "create-a".into(),
                name: "A".into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
            })
            .await
            .expect("R1");
        assert_eq!(created.revision, EnvironmentRevision(1), "R1");
        let updated = application
            .update(
                &created.id,
                EnvUpdate {
                    name: Some("B".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("R2");
        assert_eq!(updated.revision, EnvironmentRevision(2), "R2");
        application.delete(&created.id).await.expect("R3");
        assert!(
            envs.get_revision(&created.id, EnvironmentRevision(1))
                .await
                .is_some(),
            "R3"
        );
        assert_eq!(registrar.registrations.lock().unwrap().len(), 2, "R1/R2");
        assert_eq!(registrar.withdrawals.lock().unwrap().len(), 1, "R3");
        assert!(application.delete("env_local").await.is_err(), "R4");
    }
}
