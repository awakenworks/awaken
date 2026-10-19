// @formal entrypoint environment.registration_intent_is_atomic_recoverable_and_idempotent
//! Environment use cases: authored definitions, committed registration delivery,
//! and Coordinator execution projections.
//!
//! The service is the only command path that coordinates authored definitions, exact
//! sandbox-policy versions, and executable registration. HTTP, Admin Assistant,
//! standalone Control, and AllInOne all reuse this implementation.

use std::sync::Arc;

/// Coordinator execution and Worker lease use cases over the executable projection.
pub mod execution;

mod registration_delivery;
pub use registration_delivery::EnvironmentRegistrationDelivery;

use awaken_environment_contract::{
    BUILTIN_LOCAL_ENVIRONMENT_ID, CreateEnvironmentCommand, CreateEnvironmentError, EnvItem,
    EnvUpdate, EnvironmentAuthor, EnvironmentConfig, EnvironmentDefinitionQueries,
    EnvironmentDefinitionStore, EnvironmentFieldUpdate, EnvironmentRegistrationOutbox,
    EnvironmentRevision, EnvironmentSandboxPolicyRef, EnvironmentStoreError,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError,
};
use awaken_provisioning_contract::{
    HostAccessPolicy, SandboxExecutionPolicy, SandboxExecutionPolicyError,
    SandboxExecutionPolicyRef, SandboxExecutionPolicyStore, SandboxExecutionPolicyVersion,
};

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentApplicationError {
    #[error(transparent)]
    Create(#[from] CreateEnvironmentError),
    #[error(transparent)]
    Registration(#[from] ExecutableEnvironmentRegistrationError),
    #[error(transparent)]
    Store(#[from] EnvironmentStoreError),
    #[error("Environment was not found")]
    NotFound,
    #[error("Archived Environment definitions are immutable")]
    Archived,
    #[error("Built-in Environment definitions are immutable")]
    BuiltinImmutable,
    #[error(transparent)]
    Policy(#[from] SandboxExecutionPolicyError),
    #[error("Sandbox execution policy store is unavailable")]
    PolicyStoreUnavailable,
    #[error("Environment registration outbox invariant failed: {0}")]
    RegistrationInvariant(String),
}

pub struct EnvironmentApplication {
    queries: Arc<dyn EnvironmentDefinitionQueries>,
    definitions: Arc<dyn EnvironmentDefinitionStore>,
    delivery: Arc<EnvironmentRegistrationDelivery>,
    sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
}

impl EnvironmentApplication {
    /// Build all role views from the same backend instance.
    #[must_use]
    pub fn new<T>(
        definitions: Arc<T>,
        registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
        sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
    ) -> Self
    where
        T: EnvironmentDefinitionQueries
            + EnvironmentDefinitionStore
            + EnvironmentRegistrationOutbox
            + 'static,
    {
        let delivery = Arc::new(EnvironmentRegistrationDelivery::new(
            definitions.clone(),
            registrar,
            sandbox_policies.clone(),
        ));
        Self::from_roles(definitions.clone(), definitions, delivery, sandbox_policies)
    }

    /// Compose the roles already selected by the process's store construction.
    #[must_use]
    fn from_roles(
        queries: Arc<dyn EnvironmentDefinitionQueries>,
        definitions: Arc<dyn EnvironmentDefinitionStore>,
        delivery: Arc<EnvironmentRegistrationDelivery>,
        sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
    ) -> Self {
        Self {
            queries,
            definitions,
            delivery,
            sandbox_policies,
        }
    }

    #[must_use]
    pub fn registration_delivery(&self) -> Arc<EnvironmentRegistrationDelivery> {
        self.delivery.clone()
    }

    #[must_use]
    pub fn with_sandbox_policies(
        &self,
        sandbox_policies: Arc<dyn SandboxExecutionPolicyStore>,
    ) -> Self {
        Self::from_roles(
            self.queries.clone(),
            self.definitions.clone(),
            Arc::new(
                self.delivery
                    .with_sandbox_policies(sandbox_policies.clone()),
            ),
            Some(sandbox_policies),
        )
    }

    pub async fn get(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, EnvironmentApplicationError> {
        if environment_id == BUILTIN_LOCAL_ENVIRONMENT_ID {
            return Ok(Some(builtin_local_environment()));
        }
        Ok(self.queries.get(environment_id).await?)
    }

    pub async fn list_active(&self) -> Result<Vec<EnvItem>, EnvironmentApplicationError> {
        let mut items = self.queries.list_active().await?;
        items.push(builtin_local_environment());
        items.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(items)
    }

    /// Management inventory, including archived definitions. Execution callers
    /// continue to use [`Self::list_active`] so lifecycle policy cannot leak into
    /// the store or HTTP adapter.
    pub async fn list_all(&self) -> Result<Vec<EnvItem>, EnvironmentApplicationError> {
        let mut items = self.queries.list_all().await?;
        items.push(builtin_local_environment());
        items.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(items)
    }

    pub async fn create(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        validate_environment_policy(
            self.sandbox_policies.as_deref(),
            &command.config,
            command.sandbox_policy.as_ref(),
        )
        .await?;
        let item = self.definitions.create_once(command).await?.into_item();
        self.delivery.deliver_revision(&item, false).await?;
        Ok(item)
    }

    pub async fn update(
        &self,
        environment_id: &str,
        patch: EnvUpdate,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        self.ensure_mutable(environment_id)?;
        let mut candidate = self
            .queries
            .get(environment_id)
            .await?
            .ok_or(EnvironmentApplicationError::NotFound)?;
        let expected_revision = candidate.revision;
        candidate
            .apply(patch.clone())
            .map_err(EnvironmentStoreError::from)?;
        validate_environment_policy(
            self.sandbox_policies.as_deref(),
            &candidate.config,
            candidate.sandbox_policy.as_ref(),
        )
        .await?;
        let item = self
            .update_active(environment_id, expected_revision, patch)
            .await?;
        self.delivery.deliver_revision(&item, false).await?;
        Ok(item)
    }

    pub async fn archive(
        &self,
        environment_id: &str,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        self.ensure_mutable(environment_id)?;
        let item = self
            .definitions
            .archive(environment_id)
            .await?
            .ok_or(EnvironmentApplicationError::NotFound)?;
        self.delivery.deliver_revision(&item, false).await?;
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
        self.ensure_active(environment_id).await?;
        let current = self
            .queries
            .get(environment_id)
            .await?
            .ok_or(EnvironmentApplicationError::NotFound)?;
        validate_environment_policy(
            self.sandbox_policies.as_deref(),
            &current.config,
            Some(&EnvironmentSandboxPolicyRef {
                policy_id: reference.id.0.clone(),
                version: reference.version.0,
            }),
        )
        .await?;
        let item = self
            .update_active(
                environment_id,
                current.revision,
                EnvUpdate {
                    sandbox_policy: Some(EnvironmentFieldUpdate::Replace(
                        EnvironmentSandboxPolicyRef {
                            policy_id: reference.id.0,
                            version: reference.version.0,
                        },
                    )),
                    ..Default::default()
                },
            )
            .await?;
        self.delivery.deliver_revision(&item, false).await?;
        Ok(item)
    }

    async fn update_active(
        &self,
        environment_id: &str,
        expected_revision: EnvironmentRevision,
        patch: EnvUpdate,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        self.ensure_mutable(environment_id)?;
        match self
            .definitions
            .update(environment_id, expected_revision, patch)
            .await?
        {
            Some(item) => Ok(item),
            None => match self.queries.get(environment_id).await? {
                Some(item) if item.archived_at.is_some() => {
                    Err(EnvironmentApplicationError::Archived)
                }
                _ => Err(EnvironmentApplicationError::NotFound),
            },
        }
    }

    async fn ensure_active(&self, environment_id: &str) -> Result<(), EnvironmentApplicationError> {
        self.ensure_mutable(environment_id)?;
        match self.queries.get(environment_id).await? {
            Some(item) if item.archived_at.is_some() => Err(EnvironmentApplicationError::Archived),
            Some(_) => Ok(()),
            None => Err(EnvironmentApplicationError::NotFound),
        }
    }

    fn ensure_mutable(&self, environment_id: &str) -> Result<(), EnvironmentApplicationError> {
        if environment_id == BUILTIN_LOCAL_ENVIRONMENT_ID {
            Err(EnvironmentApplicationError::BuiltinImmutable)
        } else {
            Ok(())
        }
    }

    pub async fn reconcile_registration(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, EnvironmentApplicationError> {
        self.delivery.reconcile_registration(environment_id).await
    }

    pub async fn recover_registration_intents(&self) -> Result<u64, EnvironmentApplicationError> {
        self.delivery.recover_registration_intents().await
    }

    pub async fn drain_registration_intents(&self) -> Result<u64, EnvironmentApplicationError> {
        self.delivery.drain_registration_intents().await
    }
}

pub(crate) async fn validate_environment_policy(
    sandbox_policies: Option<&dyn SandboxExecutionPolicyStore>,
    config: &EnvironmentConfig,
    reference: Option<&EnvironmentSandboxPolicyRef>,
) -> Result<Option<SandboxExecutionPolicy>, EnvironmentApplicationError> {
    let Some(reference) = reference else {
        return Ok(None);
    };
    let store = sandbox_policies.ok_or(EnvironmentApplicationError::PolicyStoreUnavailable)?;
    let policy = store
        .get_exact(&SandboxExecutionPolicyRef {
            id: awaken_provisioning_contract::SandboxExecutionPolicyId(reference.policy_id.clone()),
            version: SandboxExecutionPolicyVersion(reference.version),
        })
        .await?;
    if policy.disabled {
        return Err(EnvironmentApplicationError::Policy(
            SandboxExecutionPolicyError::Disabled,
        ));
    }
    policy.validate()?;
    if policy.config.host_access == HostAccessPolicy::TrustedHost && !config.is_self_hosted() {
        return Err(EnvironmentApplicationError::Policy(
            SandboxExecutionPolicyError::Invalid(
                "trusted_host access requires a self_hosted Environment".into(),
            ),
        ));
    }
    Ok(Some(policy))
}

#[must_use]
pub fn default_environment_registration() -> ExecutableEnvironmentRegistration {
    ExecutableEnvironmentRegistration::new(builtin_local_environment(), None)
}

#[must_use]
pub fn builtin_local_environment() -> EnvItem {
    EnvItem {
        id: BUILTIN_LOCAL_ENVIRONMENT_ID.into(),
        revision: EnvironmentRevision(1),
        name: "Local".into(),
        description: Some("Built-in local execution Environment".into()),
        metadata: Default::default(),
        scope: None,
        config: EnvironmentConfig::SelfHosted,
        sandbox_policy: None,
        // The built-in local Environment exists by installation: nothing
        // registered it, so there is no acceptance or commit instant to report.
        created_at: None,
        updated_at: None,
        archived_at: None,
    }
}

#[async_trait::async_trait]
impl EnvironmentAuthor for EnvironmentApplication {
    async fn create_environment(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<String, String> {
        EnvironmentApplication::create(self, command)
            .await
            .map(|item| item.id)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use awaken_environment_contract::{
        CreateEnvironmentCommand, EnvironmentConfig, EnvironmentRegistrationIntentFilter,
        EnvironmentRegistrationOperation,
    };
    use awaken_executable_environment_contract::{
        ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentWithdrawal,
        ExecutableEnvironmentWithdrawalOutcome,
    };

    use super::*;

    fn create_command(command_id: &str, name: &str) -> CreateEnvironmentCommand {
        CreateEnvironmentCommand {
            command_id: command_id.into(),
            name: name.into(),
            description: None,
            metadata: Default::default(),
            scope: None,
            config: EnvironmentConfig::SelfHosted,
            sandbox_policy: None,
        }
    }

    #[derive(Default)]
    struct RecordingRegistrar {
        fail_registration: AtomicBool,
        fail_withdrawal: AtomicBool,
        failed_environment: Mutex<Option<String>>,
        registrations: Mutex<Vec<ExecutableEnvironmentRegistration>>,
        histories: Mutex<Vec<ExecutableEnvironmentRegistration>>,
        withdrawals: Mutex<Vec<ExecutableEnvironmentWithdrawal>>,
    }

    struct OnePolicy;

    #[async_trait]
    impl SandboxExecutionPolicyStore for OnePolicy {
        async fn create(
            &self,
            _policy: awaken_provisioning_contract::SandboxExecutionPolicy,
        ) -> Result<(), SandboxExecutionPolicyError> {
            Ok(())
        }

        async fn publish(
            &self,
            _expected_current: SandboxExecutionPolicyVersion,
            _policy: awaken_provisioning_contract::SandboxExecutionPolicy,
        ) -> Result<(), SandboxExecutionPolicyError> {
            Ok(())
        }

        async fn get_exact(
            &self,
            reference: &SandboxExecutionPolicyRef,
        ) -> Result<awaken_provisioning_contract::SandboxExecutionPolicy, SandboxExecutionPolicyError>
        {
            Ok(awaken_provisioning_contract::SandboxExecutionPolicy {
                id: reference.id.clone(),
                version: reference.version,
                config: Default::default(),
                provisioning: Default::default(),
                idle_retention: Default::default(),
                disabled: false,
            })
        }
    }

    #[async_trait]
    impl ExecutableEnvironmentRegistrar for RecordingRegistrar {
        async fn restore_historical_registration(
            &self,
            registration: ExecutableEnvironmentRegistration,
        ) -> Result<(), ExecutableEnvironmentRegistrationError> {
            registration.validate()?;
            self.histories.lock().unwrap().push(registration);
            Ok(())
        }

        async fn register(
            &self,
            registration: ExecutableEnvironmentRegistration,
        ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
        {
            if self.fail_registration.load(Ordering::SeqCst) {
                return Err(ExecutableEnvironmentRegistrationError::Unavailable(
                    "injected projection outage".into(),
                ));
            }
            if self
                .failed_environment
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|name| name == registration.definition.name)
            {
                return Err(ExecutableEnvironmentRegistrationError::Unavailable(
                    "injected unrelated projection outage".into(),
                ));
            }
            self.registrations.lock().unwrap().push(registration);
            Ok(ExecutableEnvironmentRegistrationOutcome::RegisteredCurrent)
        }

        async fn withdraw(
            &self,
            withdrawal: ExecutableEnvironmentWithdrawal,
        ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
        {
            if self.fail_withdrawal.load(Ordering::SeqCst) {
                return Err(ExecutableEnvironmentRegistrationError::Unavailable(
                    "injected withdrawal outage".into(),
                ));
            }
            self.withdrawals.lock().unwrap().push(withdrawal);
            Ok(ExecutableEnvironmentWithdrawalOutcome::WithdrawnCurrent)
        }
    }

    #[tokio::test]
    async fn authoring_delivery_and_execution_share_committed_revisions_without_sharing_authority()
    {
        use crate::execution::{CoordinatorEnvironmentRegistrar, EnvironmentExecutionApplication};
        use awaken_executable_environment_catalog::{
            ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
        };

        // Decision table: R1 authored create -> exact execution projection and
        // one healthcheck; R2 update -> execution v2 with immutable authored v1;
        // R3 archive -> history retained, new admission withdrawn, healthcheck removed;
        // R4 outbox recovery -> same terminal outcome, no new authored revision.
        let clock = awaken_time::system_clock();
        let definitions = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(clock.clone()));
        let work = Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock));
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let registrar = Arc::new(CoordinatorEnvironmentRegistrar::new(
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
            work.clone(),
        ));
        let authoring = EnvironmentApplication::new(definitions.clone(), registrar, None);
        let execution = EnvironmentExecutionApplication::new(work.clone(), catalog);
        let created = authoring
            .create(create_command("one-boundary", "initial"))
            .await
            .unwrap();
        assert_eq!(
            execution.get(&created.id).await.unwrap(),
            Some(created.clone()),
            "R1"
        );
        assert_eq!(
            execution
                .work_queries()
                .list(&created.id)
                .await
                .unwrap()
                .len(),
            1,
            "R1"
        );
        let updated = authoring
            .update(
                &created.id,
                EnvUpdate {
                    name: Some("updated".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            execution.get(&created.id).await.unwrap(),
            Some(updated.clone()),
            "R2"
        );
        assert_eq!(
            definitions
                .get_revision(&created.id, created.revision)
                .await
                .unwrap(),
            Some(created.clone()),
            "R2"
        );
        let archived = authoring.archive(&created.id).await.unwrap();
        assert_eq!(execution.get(&created.id).await.unwrap(), None, "R3");
        assert!(
            matches!(
                execution.work_queries().list(&created.id).await,
                Ok(items) if items.is_empty()
            ),
            "R3"
        );
        assert!(
            awaken_executable_environment_contract::WorkQueries::list(work.as_ref(), &created.id)
                .await
                .unwrap()
                .is_empty(),
            "R3"
        );
        authoring
            .registration_delivery()
            .recover_registration_intents()
            .await
            .unwrap();
        assert_eq!(execution.get(&created.id).await.unwrap(), None, "R4");
        assert_eq!(
            definitions.get(&created.id).await.unwrap(),
            Some(archived),
            "R4"
        );
        assert_eq!(
            definitions
                .registration_intents(EnvironmentRegistrationIntentFilter::All)
                .await
                .unwrap()
                .len(),
            3,
            "R4"
        );
    }

    #[tokio::test]
    async fn registration_delivery_recovers_without_the_authoring_application() {
        // Decision table: R1 committed create + projection outage -> one pending
        // intent; R2 authoring application dropped + recovered delivery -> same
        // revision acknowledged; R3 repeat delivery -> no new projection or revision.
        // The drainer has no definition mutation role and does not need an app back-reference.
        let definitions = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let registrar = Arc::new(RecordingRegistrar::default());
        registrar.fail_registration.store(true, Ordering::SeqCst);
        let application = EnvironmentApplication::new(definitions.clone(), registrar.clone(), None);
        assert!(
            application
                .create(create_command("independent-delivery", "one"))
                .await
                .is_err()
        );
        let delivery = application.registration_delivery();
        drop(application);
        let pending = definitions
            .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "R1");
        let intent = &pending[0];
        registrar.fail_registration.store(false, Ordering::SeqCst);
        assert_eq!(
            delivery.drain_registration_intents().await.unwrap(),
            1,
            "R2"
        );
        assert_eq!(
            delivery.drain_registration_intents().await.unwrap(),
            0,
            "R3"
        );
        assert_eq!(
            definitions
                .get(&intent.environment_id)
                .await
                .unwrap()
                .unwrap()
                .revision,
            intent.revision
        );
        assert_eq!(
            definitions
                .registration_intents(EnvironmentRegistrationIntentFilter::All)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(registrar.registrations.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn static_history_and_executable_projection_have_one_command_path() {
        // Cause/effect decision table:
        // R1 valid create -> Control revision 1 + one exact registration;
        // R2 update -> append revision 2 + register it without overwriting v1;
        // R3 delete -> append terminal revision 3 + one withdrawal, retain history;
        // R4 built-in mutation -> reject without store or projection effect.
        // Constraints/invariants: the Control history is authoritative and the
        // executable projection is driven only by its immutable command intents.
        let envs = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let registrar = Arc::new(RecordingRegistrar::default());
        let application = EnvironmentApplication::new(envs.clone(), registrar.clone(), None);
        let created = application
            .create(CreateEnvironmentCommand {
                command_id: "create-a".into(),
                name: "A".into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
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
                .expect("read Environment revision")
                .is_some(),
            "R3"
        );
        assert_eq!(registrar.registrations.lock().unwrap().len(), 2, "R1/R2");
        assert_eq!(registrar.withdrawals.lock().unwrap().len(), 1, "R3");
        assert!(application.delete("env_local").await.is_err(), "R4");
    }

    #[tokio::test]
    async fn authority_commit_survives_projection_failure_and_reconciliation_repairs_it() {
        // FMECA: F1 registrar outage after authority commit (S8/O4/D3, RPN96)
        // previously depended on a second history scan; mitigation is one pending
        // outbox fact committed with the revision. F2 acknowledgement loss after
        // an idempotent registration (S5/O4/D2, RPN40) may redeliver but cannot
        // mint a revision. F3 process restart with an empty local catalog
        // (S7/O3/D2, RPN42) replays `All` from that same log, not current rows.
        // Causes: C1 Control commit; C2 registration unavailable; C3 boundary
        // recovers; C4 process projection is empty. Effects: E1 caller sees the
        // boundary failure; E2 exact pending intent survives; E3 pending drain
        // registers and acknowledges once; E4 normal drain becomes empty; E5
        // startup recovery can replay delivered history plus built-in.
        //
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | R1   | T  | T  | F  | F  | E1,E2 |
        // | R2   | T  | F  | T  | F  | E3 |
        // | R3   | T  | F  | T  | F  | E4 |
        // | R4   | T  | F  | T  | T  | E5 |
        // Constraints/invariants: the authority commit precedes projection,
        // and recovery drains the same durable intent without minting revisions.
        let envs = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let registrar = Arc::new(RecordingRegistrar::default());
        registrar.fail_registration.store(true, Ordering::SeqCst);
        let application = EnvironmentApplication::new(envs.clone(), registrar.clone(), None);

        let result = application
            .create(CreateEnvironmentCommand {
                command_id: "projection-outage".into(),
                name: "Retained".into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
            })
            .await;
        assert!(
            matches!(result, Err(EnvironmentApplicationError::Registration(_))),
            "R1"
        );
        assert_eq!(
            envs.list_all().await.expect("list Environments").len(),
            1,
            "R1 authority retained"
        );
        assert!(registrar.registrations.lock().unwrap().is_empty(), "R1/E2");
        assert_eq!(
            envs.registration_intents(EnvironmentRegistrationIntentFilter::Pending)
                .await
                .unwrap()
                .len(),
            1,
            "R1/E2"
        );

        registrar.fail_registration.store(false, Ordering::SeqCst);
        assert_eq!(
            application.drain_registration_intents().await.unwrap(),
            1,
            "R2"
        );
        {
            let registrations = registrar.registrations.lock().unwrap();
            assert_eq!(registrations.len(), 1, "R2");
            assert!(
                registrations
                    .iter()
                    .any(|registration| registration.definition.name == "Retained"),
                "R2 exact retained revision"
            );
        }
        assert_eq!(
            application.drain_registration_intents().await.unwrap(),
            0,
            "R3"
        );
        assert_eq!(
            application.recover_registration_intents().await.unwrap(),
            2,
            "R4"
        );
    }

    #[tokio::test]
    async fn acknowledgement_loss_and_ambiguous_command_retry_converge_without_new_facts() {
        // FMECA: F1 projection succeeds but outbox acknowledgement is lost
        // (S7/O4/D5, RPN140); mitigation is idempotent exact redelivery. F2 the
        // caller retries an ambiguously completed create/update/archive
        // (S8/O5/D4, RPN160); mitigation is command identity plus no-op revision
        // detection. F3 a terminal retry resurrects execution (S10/O2/D5,
        // RPN100); mitigation is the frozen Withdraw operation.
        //
        // Cause/effect graph: C1 authority fact commits; C2 registrar succeeds;
        // C3 ack fails; C4 same command/patch/archive is retried. E1 first call
        // reports outbox failure while its intent stays pending; E2 retry may
        // redeliver but creates no revision/intent; E3 ack converges to delivered;
        // E4 archive retry remains the same terminal revision.
        //
        // | Rule | operation | C2 | C3 | C4 | effects |
        // | A1 | create | T | T | F | E1 |
        // | A2 | create | T | F | T | E2,E3 |
        // | A3 | update/no-op | T | T/F | T | E2,E3 |
        // | A4 | archive | T | T/F | T | E2,E3,E4 |
        let registry = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let envs = registry.clone();
        let registrar = Arc::new(RecordingRegistrar::default());
        let application = EnvironmentApplication::new(envs.clone(), registrar.clone(), None);

        registry.fail_next_acknowledgements(1);
        assert!(
            matches!(
                application.create(create_command("ambiguous", "v1")).await,
                Err(EnvironmentApplicationError::Store(_))
            ),
            "A1"
        );
        let created = application
            .create(create_command("ambiguous", "v1"))
            .await
            .expect("A2");
        assert_eq!(created.revision, EnvironmentRevision(1), "A2");
        assert_eq!(
            registrar.registrations.lock().unwrap().len(),
            2,
            "A1/A2 at-least-once"
        );

        registrar.fail_registration.store(true, Ordering::SeqCst);
        assert!(
            application
                .update(
                    &created.id,
                    EnvUpdate {
                        name: Some("v2".into()),
                        ..Default::default()
                    },
                )
                .await
                .is_err(),
            "A3 pending update"
        );
        registrar.fail_registration.store(false, Ordering::SeqCst);
        let replayed_update = application
            .update(
                &created.id,
                EnvUpdate {
                    name: Some("v2".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("A3 no-op retry drains existing intent");
        assert_eq!(replayed_update.revision, EnvironmentRevision(2), "A3");

        registry.fail_next_acknowledgements(1);
        assert!(
            matches!(
                application.archive(&created.id).await,
                Err(EnvironmentApplicationError::Store(_))
            ),
            "A4 first terminal delivery succeeded but ack failed"
        );
        let terminal = application.archive(&created.id).await.expect("A4 retry");
        assert_eq!(terminal.revision, EnvironmentRevision(3), "A4");
        let all = envs
            .registration_intents(EnvironmentRegistrationIntentFilter::All)
            .await
            .unwrap();
        assert_eq!(all.len(), 3, "A2/A3/A4 exactly one intent per fact");
        assert!(all.iter().all(|intent| intent.delivered), "A3/A4");
        assert_eq!(
            registrar.withdrawals.lock().unwrap().len(),
            2,
            "A4 at-least-once"
        );
    }

    struct BarrierRegistrar {
        barrier: tokio::sync::Barrier,
        registrations: Mutex<Vec<ExecutableEnvironmentRegistration>>,
    }

    #[async_trait]
    impl ExecutableEnvironmentRegistrar for BarrierRegistrar {
        async fn restore_historical_registration(
            &self,
            registration: ExecutableEnvironmentRegistration,
        ) -> Result<(), ExecutableEnvironmentRegistrationError> {
            registration.validate()
        }

        async fn register(
            &self,
            registration: ExecutableEnvironmentRegistration,
        ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
        {
            self.barrier.wait().await;
            self.registrations.lock().unwrap().push(registration);
            Ok(ExecutableEnvironmentRegistrationOutcome::RegisteredCurrent)
        }

        async fn withdraw(
            &self,
            _withdrawal: ExecutableEnvironmentWithdrawal,
        ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
        {
            unreachable!("concurrency case contains only Register")
        }
    }

    #[tokio::test]
    async fn concurrent_drainers_preserve_at_least_once_delivery_and_one_authority_fact() {
        // FMECA: F1 two supervisors read the same pending row (S5/O4/D3,
        // RPN60) -> duplicate delivery is allowed at the idempotent registrar;
        // F2 competing acknowledgements lose the fact (S8/O2/D5, RPN80) -> ack
        // is monotonic and repeatable; F3 concurrency mints a second revision
        // (S8/O2/D4, RPN64) -> only the authority transaction creates facts.
        // Decision table: R1(two reads before either ack) -> two identical
        // attempts, one durable intent, pending=0; R2(next drain) -> zero work.
        let registry = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let created = registry
            .create_once(create_command("concurrent", "one"))
            .await
            .unwrap()
            .item()
            .clone();
        let envs = registry.clone();
        let registrar = Arc::new(BarrierRegistrar {
            barrier: tokio::sync::Barrier::new(2),
            registrations: Mutex::new(Vec::new()),
        });
        let application = Arc::new(EnvironmentApplication::new(
            envs.clone(),
            registrar.clone(),
            None,
        ));
        let (left, right) = tokio::join!(
            application.drain_registration_intents(),
            application.drain_registration_intents()
        );
        assert_eq!(left.unwrap(), 1, "R1");
        assert_eq!(right.unwrap(), 1, "R1");
        {
            let attempts = registrar.registrations.lock().unwrap();
            assert_eq!(attempts.len(), 2, "R1 at-least-once");
            assert!(
                attempts
                    .iter()
                    .all(|attempt| attempt.definition.id == created.id),
                "R1 exact fact"
            );
        }
        assert_eq!(
            envs.registration_intents(EnvironmentRegistrationIntentFilter::All)
                .await
                .unwrap()
                .len(),
            1,
            "R1 one authority fact"
        );
        assert_eq!(
            application.drain_registration_intents().await.unwrap(),
            0,
            "R2"
        );
    }

    #[tokio::test]
    async fn terminal_withdrawal_uses_the_same_durable_retry_path() {
        // FMECA: F1 archive commits while Coordinator is unavailable
        // (S9/O3/D4, RPN108) -> the exact terminal Withdraw remains pending;
        // F2 retry accidentally republishes the prior active revision
        // (S10/O2/D5, RPN100) -> operation is frozen in the outbox; F3 repeated
        // archive mints another tombstone (S6/O3/D3, RPN54) -> archive returns the
        // existing terminal revision and its one intent.
        // Cause/effect graph and decision table:
        // | Rule | archived | withdrawal up | retry | effect |
        // | A1 | no  | yes | no  | v2 Withdraw delivered |
        // | A2 | no  | no  | no  | v2 Withdraw pending, caller error |
        // | A3 | yes | yes | yes | same v2 delivered, no registration |
        // | A4 | yes | yes | archive again | same v2, no new intent |
        // Effects: A1-A4 converge on one terminal revision and one withdrawal.
        // Constraints/invariants: archive is idempotent and a frozen Withdraw
        // can never be replayed as an executable registration.
        let envs = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let registrar = Arc::new(RecordingRegistrar::default());
        let application = EnvironmentApplication::new(envs.clone(), registrar.clone(), None);
        let created = application
            .create(CreateEnvironmentCommand {
                command_id: "withdrawal-outage".into(),
                name: "terminal".into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
            })
            .await
            .unwrap();
        registrar.fail_withdrawal.store(true, Ordering::SeqCst);
        assert!(
            matches!(
                application.archive(&created.id).await,
                Err(EnvironmentApplicationError::Registration(_))
            ),
            "A2"
        );
        let terminal = envs
            .get(&created.id)
            .await
            .expect("read terminal Environment")
            .unwrap();
        assert_eq!(terminal.revision, EnvironmentRevision(2), "A2");
        let pending = envs
            .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "A2 create was acknowledged");
        assert_eq!(
            pending[0].operation,
            EnvironmentRegistrationOperation::Withdraw,
            "A2"
        );

        registrar.fail_withdrawal.store(false, Ordering::SeqCst);
        assert_eq!(
            application.drain_registration_intents().await.unwrap(),
            1,
            "A3"
        );
        assert!(
            registrar
                .registrations
                .lock()
                .unwrap()
                .iter()
                .all(|registration| { registration.definition.revision == EnvironmentRevision(1) }),
            "A3"
        );
        let replay = application.archive(&created.id).await.unwrap();
        assert_eq!(replay.revision, EnvironmentRevision(2), "A4");
        assert_eq!(
            envs.registration_intents(EnvironmentRegistrationIntentFilter::All)
                .await
                .unwrap()
                .len(),
            2,
            "A4"
        );
    }

    #[tokio::test]
    async fn exact_reconciliation_isolated_from_unrelated_registration_failure() {
        // FMECA: F1 one poison registration stops the ordered batch and starves
        // later Environments (S8/O4/D5, RPN160). Mitigation: attempt every
        // immutable intent, retain the first error for observability, and leave
        // only failed acknowledgements pending. F2 dispatch readiness scans the
        // whole catalog (S7/O4/D4, RPN112). Mitigation: exact reconciliation.
        // Decision table:
        // R1 full replay + first custom Environment fails -> replay reports the
        // failure but still registers the later healthy Environment;
        // R2 exact healthy id + same unrelated failure -> healthy registration succeeds;
        // R3 unknown id -> no registration and an explicit missing result.
        let envs = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let registrar = Arc::new(RecordingRegistrar::default());
        let application = EnvironmentApplication::new(envs, registrar.clone(), None);
        application
            .create(create_command("unrelated", "Unrelated"))
            .await
            .unwrap();
        let healthy = application
            .create(create_command("healthy", "Healthy"))
            .await
            .unwrap();
        registrar.registrations.lock().unwrap().clear();
        *registrar.failed_environment.lock().unwrap() = Some("Unrelated".into());

        assert!(
            application.recover_registration_intents().await.is_err(),
            "R1"
        );
        assert!(
            registrar
                .registrations
                .lock()
                .unwrap()
                .iter()
                .any(|registration| registration.definition.name == "Healthy"),
            "R1 failed predecessor does not starve later work"
        );
        registrar.registrations.lock().unwrap().clear();
        assert_eq!(
            application
                .reconcile_registration(&healthy.id)
                .await
                .unwrap()
                .map(|item| item.id),
            Some(healthy.id.clone()),
            "R2"
        );
        assert_eq!(
            registrar
                .registrations
                .lock()
                .unwrap()
                .iter()
                .map(|registration| registration.definition.name.as_str())
                .collect::<Vec<_>>(),
            ["Healthy"],
            "R2"
        );
        assert!(
            application
                .reconcile_registration("env_missing")
                .await
                .unwrap()
                .is_none(),
            "R3"
        );
    }

    #[tokio::test]
    async fn archive_is_terminal_for_every_environment_mutation_path() {
        // Cause/effect graph: C1 an active definition can update/bind; C2 archive
        // appends one tombstone and withdraws execution; C3 any later definition
        // update; C4 any later sandbox-policy bind. Effects: E1 C3/C4 return the
        // same typed lifecycle conflict, E2 no revision is appended, E3 no
        // registration recreates current execution availability.
        //
        // | Rule | archived | mutation | result | revision/projection effect |
        // | T1 | false | create | success | revision 1 + one registration |
        // | T2 | false | archive | success | revision 2 + one withdrawal |
        // | T3 | true | update | Archived | none |
        // | T4 | true | bind policy | Archived | none |
        // Constraints/invariants: the tombstone is absorbing for every mutation
        // path and neither revision nor executable projection may resurrect.
        let envs = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let registrar = Arc::new(RecordingRegistrar::default());
        let application =
            EnvironmentApplication::new(envs.clone(), registrar.clone(), Some(Arc::new(OnePolicy)));
        let created = application
            .create(CreateEnvironmentCommand {
                command_id: "terminal-mutations".into(),
                name: "terminal".into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
            })
            .await
            .expect("T1");
        application.archive(&created.id).await.expect("T2");
        assert!(
            application
                .list_active()
                .await
                .unwrap()
                .iter()
                .all(|item| item.id != created.id),
            "T2 execution inventory excludes the tombstone"
        );
        assert!(
            application
                .list_all()
                .await
                .unwrap()
                .iter()
                .any(|item| item.id == created.id && item.archived_at.is_some()),
            "T2 management inventory retains the archived definition"
        );

        assert!(
            matches!(
                application
                    .update(
                        &created.id,
                        EnvUpdate {
                            name: Some("revived".into()),
                            ..Default::default()
                        }
                    )
                    .await,
                Err(EnvironmentApplicationError::Archived)
            ),
            "T3"
        );
        assert!(
            matches!(
                application
                    .bind_sandbox_policy(
                        &created.id,
                        SandboxExecutionPolicyRef {
                            id: awaken_provisioning_contract::SandboxExecutionPolicyId(
                                "policy".into(),
                            ),
                            version: SandboxExecutionPolicyVersion(1),
                        }
                    )
                    .await,
                Err(EnvironmentApplicationError::Archived)
            ),
            "T4"
        );
        let terminal = envs
            .get(&created.id)
            .await
            .expect("read terminal Environment")
            .expect("terminal row");
        assert_eq!(terminal.revision, EnvironmentRevision(2), "T3/T4");
        assert_eq!(terminal.name, "terminal", "T3");
        assert!(terminal.sandbox_policy.is_none(), "T4");
        assert_eq!(registrar.registrations.lock().unwrap().len(), 1, "T3/T4");
        assert_eq!(registrar.withdrawals.lock().unwrap().len(), 1, "T3/T4");
    }
    #[tokio::test]
    async fn lost_registration_ack_then_archived_ack_cannot_replay_old_pending_work() {
        use crate::execution::CoordinatorEnvironmentRegistrar;
        use awaken_executable_environment_catalog::{
            ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
        };
        use awaken_executable_environment_contract::WorkQueries;
        for withdrawal_ack_lost in [false, true] {
            let clock = awaken_time::system_clock();
            let definitions = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(clock.clone()));
            let work = Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock.clone()));
            let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
            let application = EnvironmentApplication::new(
                definitions.clone(),
                Arc::new(CoordinatorEnvironmentRegistrar::new(
                    Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
                    work.clone(),
                )),
                None,
            );
            definitions.fail_next_acknowledgements(1);
            assert!(
                application
                    .create(create_command("lost-registration-ack", "archived"))
                    .await
                    .is_err()
            );
            let created = definitions.list_active().await.unwrap().pop().unwrap();
            assert_eq!(work.list(&created.id).await.unwrap().len(), 1);
            if withdrawal_ack_lost {
                definitions.fail_next_acknowledgements(1);
                assert!(application.archive(&created.id).await.is_err());
            }
            let archived = application.archive(&created.id).await.unwrap();
            assert_eq!(archived.revision, EnvironmentRevision(2));
            assert!(work.list(&created.id).await.unwrap().is_empty());
            let pending = definitions
                .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
                .await
                .unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].revision, EnvironmentRevision(1));
            assert_eq!(application.drain_registration_intents().await.unwrap(), 1);
            assert!(
                work.list(&created.id).await.unwrap().is_empty(),
                "stale Pending cannot resurrect healthcheck"
            );
            assert!(catalog.current(&created.id).is_none());
            assert!(
                catalog
                    .at_revision(&created.id, EnvironmentRevision(1))
                    .is_some(),
                "frozen history survives"
            );
            // A fresh empty catalog AND empty watermark simulates first upgrade
            // or volatile AllInOne restart. Highest-first All must install the
            // true withdrawal first while still restoring every historical row.
            let recovered_catalog = Arc::new(ExecutableEnvironmentCatalog::new());
            let recovered_work = Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock));
            let recovered = EnvironmentApplication::new(
                definitions.clone(),
                Arc::new(CoordinatorEnvironmentRegistrar::new(
                    Arc::new(LocalExecutableEnvironmentRegistrar::new(
                        recovered_catalog.clone(),
                    )),
                    recovered_work.clone(),
                )),
                None,
            );
            recovered.recover_registration_intents().await.unwrap();
            assert!(recovered_work.list(&created.id).await.unwrap().is_empty());
            assert!(recovered_catalog.current(&created.id).is_none());
            assert!(
                recovered_catalog
                    .at_revision(&created.id, EnvironmentRevision(1))
                    .is_some()
            );
        }
    }

    #[tokio::test]
    async fn failed_latest_all_revision_restores_old_history_without_old_admission() {
        use crate::execution::CoordinatorEnvironmentRegistrar;
        use awaken_executable_environment_contract::WorkQueries;
        let clock = awaken_time::system_clock();
        let definitions = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(clock.clone()));
        let bootstrap = Arc::new(RecordingRegistrar::default());
        let authoring = EnvironmentApplication::new(definitions.clone(), bootstrap.clone(), None);
        let archived = authoring
            .create(create_command("highest-fails", "archive-target"))
            .await
            .unwrap();
        let healthy = authoring
            .create(create_command("other-healthy", "healthy"))
            .await
            .unwrap();
        authoring.archive(&archived.id).await.unwrap();
        let delegate = Arc::new(RecordingRegistrar::default());
        delegate.fail_withdrawal.store(true, Ordering::SeqCst);
        let work = Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock));
        let recovering = EnvironmentApplication::new(
            definitions,
            Arc::new(CoordinatorEnvironmentRegistrar::new(
                delegate.clone(),
                work.clone(),
            )),
            None,
        );
        assert!(recovering.recover_registration_intents().await.is_err());
        assert!(
            work.list(&archived.id).await.unwrap().is_empty(),
            "failed highest revision cannot fall through to old Register"
        );
        assert!(
            delegate
                .registrations
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.definition.id != archived.id)
        );
        assert_eq!(
            work.list(&healthy.id).await.unwrap().len(),
            1,
            "another Environment still recovers"
        );
        assert!(
            delegate
                .histories
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.definition.id == archived.id),
            "latest outage must not starve frozen history"
        );
        delegate.fail_withdrawal.store(false, Ordering::SeqCst);
        recovering.recover_registration_intents().await.unwrap();
        assert!(
            delegate
                .histories
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.definition.id == archived.id),
            "retry restores historical catalog facts through history-only role"
        );
        assert!(work.list(&archived.id).await.unwrap().is_empty());
    }

    struct WithdrawalOutageRegistrar {
        inner: awaken_executable_environment_catalog::LocalExecutableEnvironmentRegistrar,
        unavailable: AtomicBool,
    }

    #[async_trait]
    impl ExecutableEnvironmentRegistrar for WithdrawalOutageRegistrar {
        async fn restore_historical_registration(
            &self,
            registration: ExecutableEnvironmentRegistration,
        ) -> Result<(), ExecutableEnvironmentRegistrationError> {
            self.inner
                .restore_historical_registration(registration)
                .await
        }
        async fn register(
            &self,
            registration: ExecutableEnvironmentRegistration,
        ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
        {
            self.inner.register(registration).await
        }
        async fn withdraw(
            &self,
            withdrawal: ExecutableEnvironmentWithdrawal,
        ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
        {
            if self.unavailable.load(Ordering::SeqCst) {
                return Err(ExecutableEnvironmentRegistrationError::Unavailable(
                    "latest withdrawal unavailable".into(),
                ));
            }
            self.inner.withdraw(withdrawal).await
        }
    }

    #[tokio::test]
    async fn pending_before_all_uses_acknowledged_latest_intent_and_keeps_failed_latest_retryable()
    {
        use crate::execution::CoordinatorEnvironmentRegistrar;
        use awaken_executable_environment_catalog::{
            ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
        };
        use awaken_executable_environment_contract::WorkQueries;
        // C1 old Register ack lost; C2 newer Withdraw already acknowledged;
        // C3 projection/watermark empty after upgrade; C4 Pending runs before All;
        // C5 latest delivery persistently fails. P1 C1-C4 -> replay true latest,
        // restore old history only, no healthcheck. P2 +C5 -> history still exists,
        // no current/queue, old Pending retained for the next latest retry.
        for fail_latest in [false, true] {
            let clock = awaken_time::system_clock();
            let definitions = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(clock.clone()));
            let authoring = EnvironmentApplication::new(
                definitions.clone(),
                Arc::new(RecordingRegistrar::default()),
                None,
            );
            definitions.fail_next_acknowledgements(1);
            assert!(
                authoring
                    .create(create_command("cold-pending", "frozen"))
                    .await
                    .is_err()
            );
            let item = definitions.list_active().await.unwrap().pop().unwrap();
            authoring.archive(&item.id).await.unwrap();
            assert!(
                definitions
                    .registration_intent(&item.id, EnvironmentRevision(2))
                    .await
                    .unwrap()
                    .unwrap()
                    .delivered
            );
            let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
            let work = Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock));
            let delegate = Arc::new(WithdrawalOutageRegistrar {
                inner: LocalExecutableEnvironmentRegistrar::new(catalog.clone()),
                unavailable: AtomicBool::new(fail_latest),
            });
            let recovering = EnvironmentApplication::new(
                definitions.clone(),
                Arc::new(CoordinatorEnvironmentRegistrar::new(
                    delegate.clone(),
                    work.clone(),
                )),
                None,
            );
            for _ in 0..2 {
                let result = recovering.drain_registration_intents().await;
                assert_eq!(result.is_err(), fail_latest, "P1/P2 exact latest outcome");
                assert!(
                    catalog.current(&item.id).is_none(),
                    "P1/P2 history never becomes current"
                );
                assert!(
                    work.list(&item.id).await.unwrap().is_empty(),
                    "P1/P2 cold watermark cannot permit old work"
                );
                assert!(
                    catalog
                        .at_revision(&item.id, EnvironmentRevision(1))
                        .is_some(),
                    "P1/P2 frozen history always recovers"
                );
                assert_eq!(
                    definitions
                        .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
                        .await
                        .unwrap()
                        .len(),
                    usize::from(fail_latest),
                    "P2 retain retry obligation"
                );
            }
            delegate.unavailable.store(false, Ordering::SeqCst);
            recovering.drain_registration_intents().await.unwrap();
            assert!(
                definitions
                    .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(work.list(&item.id).await.unwrap().is_empty());
        }
    }
}
