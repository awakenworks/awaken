//! Canonical Control application service for static Environment definitions.
//!
//! The service is the only command path that coordinates `EnvRegistry`, exact
//! sandbox-policy versions, and executable registration. HTTP, Admin Assistant,
//! standalone Control, and AllInOne all reuse this implementation.

use std::sync::Arc;

use awaken_environment_contract::{
    BUILTIN_LOCAL_ENVIRONMENT_ID, CreateEnvironmentCommand, CreateEnvironmentError, EnvItem,
    EnvRegistry, EnvUpdate, EnvironmentAuthor, EnvironmentConfig, EnvironmentFieldUpdate,
    EnvironmentRegistrationIntent, EnvironmentRegistrationIntentFilter,
    EnvironmentRegistrationOperation, EnvironmentRevision, EnvironmentSandboxPolicyRef,
    EnvironmentStoreError,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentWithdrawal,
};
use awaken_provisioning_contract::{
    SandboxExecutionPolicyError, SandboxExecutionPolicyRef, SandboxExecutionPolicyStore,
    SandboxExecutionPolicyVersion,
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
                    .await?,
            ),
            (None, Some(_)) => {
                return Err(EnvironmentApplicationError::PolicyStoreUnavailable);
            }
            (_, None) => None,
        };
        Ok(ExecutableEnvironmentRegistration::new(item, sandbox_policy))
    }

    async fn deliver_intent(
        &self,
        intent: EnvironmentRegistrationIntent,
        replay_delivered: bool,
    ) -> Result<(), EnvironmentApplicationError> {
        if intent.delivered && !replay_delivered {
            return Ok(());
        }
        match intent.operation {
            EnvironmentRegistrationOperation::Register => {
                let item = self
                    .envs
                    .get_revision(&intent.environment_id, intent.revision)
                    .await?
                    .ok_or_else(|| {
                        EnvironmentApplicationError::RegistrationInvariant(format!(
                            "missing revision {}@{}",
                            intent.environment_id, intent.revision.0
                        ))
                    })?;
                self.registrar
                    .register(self.registration(item).await?)
                    .await?;
            }
            EnvironmentRegistrationOperation::Withdraw => {
                self.registrar
                    .withdraw(ExecutableEnvironmentWithdrawal {
                        environment_id: intent.environment_id.clone(),
                        lifecycle_revision: intent.revision,
                    })
                    .await?;
            }
        }
        if !self
            .envs
            .mark_registration_intent_delivered(&intent)
            .await?
        {
            return Err(EnvironmentApplicationError::RegistrationInvariant(format!(
                "missing intent {}@{}",
                intent.environment_id, intent.revision.0
            )));
        }
        Ok(())
    }

    async fn deliver_revision(
        &self,
        item: &EnvItem,
        replay_delivered: bool,
    ) -> Result<(), EnvironmentApplicationError> {
        let intent = self
            .envs
            .registration_intent(&item.id, item.revision)
            .await?
            .ok_or_else(|| {
                EnvironmentApplicationError::RegistrationInvariant(format!(
                    "missing intent {}@{}",
                    item.id, item.revision.0
                ))
            })?;
        self.deliver_intent(intent, replay_delivered).await
    }

    pub async fn get(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, EnvironmentApplicationError> {
        if environment_id == BUILTIN_LOCAL_ENVIRONMENT_ID {
            return Ok(Some(builtin_local_environment()));
        }
        Ok(self.envs.get(environment_id).await?)
    }

    pub async fn list_active(&self) -> Result<Vec<EnvItem>, EnvironmentApplicationError> {
        let mut items = self.envs.list_active().await?;
        items.push(builtin_local_environment());
        items.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(items)
    }

    pub async fn create(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        let item = self.envs.create_once(command).await?.into_item();
        self.deliver_revision(&item, false).await?;
        Ok(item)
    }

    /// Reconcile one exact Environment into the rebuildable executable projection.
    ///
    /// Dispatch readiness is scoped to one frozen Environment activation. It must
    /// not replay the entire catalog: an unrelated package-image registration may
    /// be slow or unavailable without invalidating an already-selected Environment.
    /// The background registration supervisor remains responsible for full-catalog
    /// recovery.
    pub async fn reconcile_registration(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, EnvironmentApplicationError> {
        if environment_id == BUILTIN_LOCAL_ENVIRONMENT_ID {
            self.registrar
                .register(default_environment_registration())
                .await?;
            return Ok(Some(builtin_local_environment()));
        }
        let Some(current) = self.envs.get(environment_id).await? else {
            return Ok(None);
        };
        self.deliver_revision(&current, true).await?;
        Ok(Some(current))
    }

    async fn deliver_registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
        replay_delivered: bool,
    ) -> Result<u64, EnvironmentApplicationError> {
        let intents = self.envs.registration_intents(filter).await?;
        let mut delivered = 0_u64;
        let mut first_error = None;
        for intent in intents {
            match self.deliver_intent(intent, replay_delivered).await {
                Ok(()) => delivered = delivered.saturating_add(1),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(delivered),
        }
    }

    /// Rebuild an empty executable projection from the durable intent log. This
    /// is a startup recovery mode of the same outbox drainer, not a scan that
    /// re-derives work from mutable Environment rows.
    pub async fn recover_registration_intents(&self) -> Result<u64, EnvironmentApplicationError> {
        self.registrar
            .register(default_environment_registration())
            .await?;
        Ok(1 + self
            .deliver_registration_intents(EnvironmentRegistrationIntentFilter::All, true)
            .await?)
    }

    /// Deliver only authority-committed intents that have not yet been
    /// acknowledged by the executable projection boundary.
    pub async fn drain_registration_intents(&self) -> Result<u64, EnvironmentApplicationError> {
        self.deliver_registration_intents(EnvironmentRegistrationIntentFilter::Pending, false)
            .await
    }

    pub async fn update(
        &self,
        environment_id: &str,
        patch: EnvUpdate,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        let item = self.update_active(environment_id, patch).await?;
        self.deliver_revision(&item, false).await?;
        Ok(item)
    }

    pub async fn archive(
        &self,
        environment_id: &str,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        self.ensure_mutable(environment_id)?;
        let item = self
            .envs
            .archive(environment_id)
            .await?
            .ok_or(EnvironmentApplicationError::NotFound)?;
        self.deliver_revision(&item, false).await?;
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
        let store = self
            .sandbox_policies
            .as_ref()
            .ok_or(EnvironmentApplicationError::PolicyStoreUnavailable)?;
        let policy = store.get_exact(&reference).await?;
        if policy.disabled {
            return Err(EnvironmentApplicationError::Policy(
                SandboxExecutionPolicyError::Disabled,
            ));
        }
        let item = self
            .update_active(
                environment_id,
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
        self.deliver_revision(&item, false).await?;
        Ok(item)
    }

    async fn update_active(
        &self,
        environment_id: &str,
        patch: EnvUpdate,
    ) -> Result<EnvItem, EnvironmentApplicationError> {
        self.ensure_mutable(environment_id)?;
        match self.envs.update(environment_id, patch).await? {
            Some(item) => Ok(item),
            None => match self.envs.get(environment_id).await? {
                Some(item) if item.archived_at.is_some() => {
                    Err(EnvironmentApplicationError::Archived)
                }
                _ => Err(EnvironmentApplicationError::NotFound),
            },
        }
    }

    async fn ensure_active(&self, environment_id: &str) -> Result<(), EnvironmentApplicationError> {
        self.ensure_mutable(environment_id)?;
        match self.envs.get(environment_id).await? {
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
        description: "Built-in local execution Environment".into(),
        metadata: Default::default(),
        scope: None,
        config: EnvironmentConfig::SelfHosted,
        sandbox_policy: None,
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
    use awaken_environment_contract::{CreateEnvironmentCommand, EnvironmentConfig};
    use awaken_executable_environment_contract::{
        ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentWithdrawalOutcome,
    };

    use super::*;

    fn create_command(command_id: &str, name: &str) -> CreateEnvironmentCommand {
        CreateEnvironmentCommand {
            command_id: command_id.into(),
            name: name.into(),
            description: String::new(),
            metadata: Default::default(),
            scope: None,
            config: EnvironmentConfig::SelfHosted,
        }
    }

    #[derive(Default)]
    struct RecordingRegistrar {
        fail_registration: AtomicBool,
        fail_withdrawal: AtomicBool,
        failed_environment: Mutex<Option<String>>,
        registrations: Mutex<Vec<ExecutableEnvironmentRegistration>>,
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
    async fn static_history_and_executable_projection_have_one_command_path() {
        // Cause/effect decision table:
        // R1 valid create -> Control revision 1 + one exact registration;
        // R2 update -> append revision 2 + register it without overwriting v1;
        // R3 delete -> append terminal revision 3 + one withdrawal, retain history;
        // R4 built-in mutation -> reject without store or projection effect.
        // Constraints/invariants: the Control history is authoritative and the
        // executable projection is driven only by its immutable command intents.
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
        let envs: Arc<dyn EnvRegistry> = Arc::new(awaken_env_store::InMemoryEnvRegistry::new());
        let registrar = Arc::new(RecordingRegistrar::default());
        registrar.fail_registration.store(true, Ordering::SeqCst);
        let application = EnvironmentApplication::new(envs.clone(), registrar.clone(), None);

        let result = application
            .create(CreateEnvironmentCommand {
                command_id: "projection-outage".into(),
                name: "Retained".into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
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
        let registry = Arc::new(awaken_env_store::InMemoryEnvRegistry::new());
        let envs: Arc<dyn EnvRegistry> = registry.clone();
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
        let registry = Arc::new(awaken_env_store::InMemoryEnvRegistry::new());
        let created = registry
            .create_once(create_command("concurrent", "one"))
            .await
            .unwrap()
            .item()
            .clone();
        let envs: Arc<dyn EnvRegistry> = registry.clone();
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
        let envs: Arc<dyn EnvRegistry> = Arc::new(awaken_env_store::InMemoryEnvRegistry::new());
        let registrar = Arc::new(RecordingRegistrar::default());
        let application = EnvironmentApplication::new(envs.clone(), registrar.clone(), None);
        let created = application
            .create(CreateEnvironmentCommand {
                command_id: "withdrawal-outage".into(),
                name: "terminal".into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
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
        let envs: Arc<dyn EnvRegistry> = Arc::new(awaken_env_store::InMemoryEnvRegistry::new());
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
        let envs: Arc<dyn EnvRegistry> = Arc::new(awaken_env_store::InMemoryEnvRegistry::new());
        let registrar = Arc::new(RecordingRegistrar::default());
        let application =
            EnvironmentApplication::new(envs.clone(), registrar.clone(), Some(Arc::new(OnePolicy)));
        let created = application
            .create(CreateEnvironmentCommand {
                command_id: "terminal-mutations".into(),
                name: "terminal".into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
            })
            .await
            .expect("T1");
        application.archive(&created.id).await.expect("T2");

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
}
