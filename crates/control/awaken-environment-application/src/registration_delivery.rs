// @formal surface obligation=environment.registration_intent_is_atomic_recoverable_and_idempotent
// @formal reason: Deliver and acknowledge the exact authority-committed Environment revision intent.
//! Replay authority-committed registration intents into the executable projection.

use std::sync::Arc;

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
            .outbox
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
        let intents = self.outbox.registration_intents(filter).await?;
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
}
