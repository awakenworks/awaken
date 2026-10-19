// @formal surface obligation=environment.registration_intent_is_atomic_recoverable_and_idempotent
// @formal reason: Deliver and acknowledge the exact authority-committed Environment revision intent.
//! Replay authority-committed registration intents into the executable projection.

use std::{collections::BTreeMap, sync::Arc};

use awaken_environment_contract::{
    BUILTIN_LOCAL_ENVIRONMENT_ID, EnvItem, EnvironmentDefinitionQueries,
    EnvironmentRegistrationIntent, EnvironmentRegistrationIntentFilter,
    EnvironmentRegistrationOperation, EnvironmentRegistrationOutbox,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentWithdrawal,
};
use awaken_provisioning_contract::SandboxExecutionPolicyStore;

use super::{
    EnvironmentApplicationError, builtin_local_environment, default_environment_registration,
    validate_environment_policy,
};

/// Delivery of committed Environment revisions; this component cannot author definitions.
pub struct EnvironmentRegistrationDelivery {
    queries: Arc<dyn EnvironmentDefinitionQueries>,
    outbox: Arc<dyn EnvironmentRegistrationOutbox>,
    registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
}

impl EnvironmentRegistrationDelivery {
    /// Query and outbox views are derived from one physical authority instance.
    #[must_use]
    pub fn new<T>(
        definitions: Arc<T>,
        registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
        sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
    ) -> Self
    where
        T: EnvironmentDefinitionQueries + EnvironmentRegistrationOutbox + 'static,
    {
        Self {
            queries: definitions.clone(),
            outbox: definitions,
            registrar,
            sandbox_policies,
        }
    }

    pub(crate) fn with_sandbox_policies(
        &self,
        policies: Arc<dyn SandboxExecutionPolicyStore>,
    ) -> Self {
        Self {
            queries: self.queries.clone(),
            outbox: self.outbox.clone(),
            registrar: self.registrar.clone(),
            sandbox_policies: Some(policies),
        }
    }

    async fn registration(
        &self,
        item: EnvItem,
    ) -> Result<ExecutableEnvironmentRegistration, EnvironmentApplicationError> {
        let sandbox_policy = validate_environment_policy(
            self.sandbox_policies.as_deref(),
            &item.config,
            item.sandbox_policy.as_ref(),
        )
        .await?;
        Ok(ExecutableEnvironmentRegistration::new(item, sandbox_policy))
    }

    async fn registration_for_intent(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<ExecutableEnvironmentRegistration, EnvironmentApplicationError> {
        let item = self
            .queries
            .get_revision(&intent.environment_id, intent.revision)
            .await?
            .ok_or_else(|| {
                EnvironmentApplicationError::RegistrationInvariant(format!(
                    "missing revision {}@{}",
                    intent.environment_id, intent.revision.0
                ))
            })?;
        if item.id != intent.environment_id
            || item.revision != intent.revision
            || EnvironmentRegistrationIntent::for_item(&item).operation != intent.operation
        {
            return Err(EnvironmentApplicationError::RegistrationInvariant(format!(
                "revision facts disagree with intent {}@{}",
                intent.environment_id, intent.revision.0
            )));
        }
        self.registration(item).await
    }

    async fn acknowledge(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<(), EnvironmentApplicationError> {
        if !self
            .outbox
            .mark_registration_intent_delivered(intent)
            .await?
        {
            return Err(EnvironmentApplicationError::RegistrationInvariant(format!(
                "missing intent {}@{}",
                intent.environment_id, intent.revision.0
            )));
        }
        Ok(())
    }

    async fn deliver_intent(
        &self,
        intent: EnvironmentRegistrationIntent,
        replay_delivered: bool,
    ) -> Result<(), EnvironmentApplicationError> {
        if !intent.requires_delivery(replay_delivered) {
            return Ok(());
        }
        match intent.operation {
            EnvironmentRegistrationOperation::Register => {
                self.registrar
                    .register(self.registration_for_intent(&intent).await?)
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
        self.acknowledge(&intent).await
    }

    async fn latest_intent(
        &self,
        environment_id: &str,
    ) -> Result<EnvironmentRegistrationIntent, EnvironmentApplicationError> {
        let current = self.queries.get(environment_id).await?.ok_or_else(|| {
            EnvironmentApplicationError::RegistrationInvariant(format!(
                "missing current definition {environment_id}"
            ))
        })?;
        let intent = self
            .outbox
            .registration_intent(environment_id, current.revision)
            .await?
            .ok_or_else(|| {
                EnvironmentApplicationError::RegistrationInvariant(format!(
                    "missing current intent {environment_id}@{}",
                    current.revision.0
                ))
            })?;
        let expected = EnvironmentRegistrationIntent::for_item(&current);
        if intent.environment_id != environment_id
            || current.id != environment_id
            || intent.revision != current.revision
            || intent.operation != expected.operation
        {
            return Err(EnvironmentApplicationError::RegistrationInvariant(format!(
                "current definition disagrees with intent {environment_id}@{}",
                current.revision.0
            )));
        }
        Ok(intent)
    }

    async fn restore_history(
        &self,
        intent: &EnvironmentRegistrationIntent,
        acknowledge: bool,
    ) -> Result<(), EnvironmentApplicationError> {
        if intent.operation != EnvironmentRegistrationOperation::Register {
            // Archive is terminal. An older Withdraw cannot precede a newer
            // authored definition and must not be silently treated as history.
            return Err(EnvironmentApplicationError::RegistrationInvariant(format!(
                "nonterminal historical withdrawal {}@{}",
                intent.environment_id, intent.revision.0
            )));
        }
        self.registrar
            .restore_historical_registration(self.registration_for_intent(intent).await?)
            .await?;
        if acknowledge {
            self.acknowledge(intent).await?;
        }
        Ok(())
    }

    pub(crate) async fn deliver_revision(
        &self,
        item: &EnvItem,
        replay_delivered: bool,
    ) -> Result<(), EnvironmentApplicationError> {
        let intent = self
            .outbox
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
        let Some(current) = self.queries.get(environment_id).await? else {
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
        let mut batches = BTreeMap::<String, Vec<EnvironmentRegistrationIntent>>::new();
        for intent in self.outbox.registration_intents(filter).await? {
            if intent.requires_delivery(replay_delivered) {
                batches
                    .entry(intent.environment_id.clone())
                    .or_default()
                    .push(intent);
            }
        }
        let mut delivered = 0_u64;
        let mut first_error = None;
        for (environment_id, intents) in batches {
            // Pending may contain only an old Register while the true newest
            // Withdraw is already acknowledged. Read the exact authoritative
            // current intent, never infer latest from the selected subset.
            let latest = match self.latest_intent(&environment_id).await {
                Ok(intent) => Some(intent),
                Err(error) => {
                    first_error.get_or_insert(error);
                    None
                }
            };
            let current_ready = if let Some(intent) = &latest {
                match self.deliver_intent(intent.clone(), true).await {
                    Ok(()) => true,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        false
                    }
                }
            } else {
                false
            };
            for intent in intents {
                if let Some(latest) = &latest {
                    if intent.revision == latest.revision && intent.operation == latest.operation {
                        if current_ready {
                            delivered = delivered.saturating_add(1);
                        }
                        continue;
                    }
                    if intent.revision >= latest.revision {
                        first_error.get_or_insert_with(|| {
                            EnvironmentApplicationError::RegistrationInvariant(format!(
                                "selected intent is ahead of current {environment_id}@{}",
                                intent.revision.0
                            ))
                        });
                        continue;
                    }
                }
                // Failed current build/delivery cannot starve frozen Sessions:
                // restore old exact catalog facts and builds without admission
                // or queue effects. Keep Pending until current convergence also
                // succeeds, so its retry obligation survives an earlier ack.
                match self.restore_history(&intent, current_ready).await {
                    Ok(()) if current_ready => delivered = delivered.saturating_add(1),
                    Ok(()) => {}
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(delivered),
        }
    }

    /// Rebuild an empty executable projection from the durable intent log. This
    /// is a startup recovery mode of the same outbox drainer. Current definitions
    /// select an exact committed intent; they never synthesize a queue operation.
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
}
