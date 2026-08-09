//! Coordinator-owned projection of Control's executable Environment revisions.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use awaken_environment_contract::EnvironmentRevision;
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentRegistrationSource, ExecutableEnvironmentWithdrawal,
    ExecutableEnvironmentWithdrawalOutcome,
};

mod http;
mod postgres;
mod schema;

pub use http::{
    EXECUTABLE_ENVIRONMENT_REGISTER_PATH, EXECUTABLE_ENVIRONMENT_WITHDRAW_PATH,
    HttpExecutableEnvironmentRegistrar, executable_environment_registration_router,
};
pub use postgres::PostgresExecutableEnvironmentRegistrar;
pub use schema::executable_environment_catalog_bundle;

#[derive(Clone)]
struct CurrentEntry {
    lifecycle_revision: EnvironmentRevision,
    registration: Option<ExecutableEnvironmentRegistration>,
}

#[derive(Clone, Default)]
struct CatalogState {
    current: BTreeMap<String, CurrentEntry>,
    revisions: BTreeMap<(String, EnvironmentRevision), ExecutableEnvironmentRegistration>,
}

#[derive(Default)]
pub struct ExecutableEnvironmentCatalog {
    state: RwLock<CatalogState>,
}

impl ExecutableEnvironmentCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install one deployment-supplied immutable registration through the same
    /// state machine used by local and HTTP registrars. This synchronous seam is
    /// used only while composing the built-in `env_local` fact.
    pub fn install_seed(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let mut state = self.state.write().map_err(|_| {
            ExecutableEnvironmentRegistrationError::Storage("catalog lock poisoned".into())
        })?;
        Self::register_locked(&mut state, registration)
    }

    pub fn current(&self, environment_id: &str) -> Option<ExecutableEnvironmentRegistration> {
        self.state
            .read()
            .expect("executable Environment catalog")
            .current
            .get(environment_id)
            .and_then(|entry| entry.registration.clone())
    }

    pub fn current_all(&self) -> Vec<ExecutableEnvironmentRegistration> {
        self.state
            .read()
            .expect("executable Environment catalog")
            .current
            .values()
            .filter_map(|entry| entry.registration.clone())
            .collect()
    }

    pub fn at_revision(
        &self,
        environment_id: &str,
        revision: EnvironmentRevision,
    ) -> Option<ExecutableEnvironmentRegistration> {
        self.state
            .read()
            .expect("executable Environment catalog")
            .revisions
            .get(&(environment_id.to_string(), revision))
            .cloned()
    }

    fn register_locked(
        state: &mut CatalogState,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        registration.validate()?;
        let id = registration.definition.id.clone();
        let revision = registration.definition.revision;
        let key = (id.clone(), revision);
        if let Some(existing) = state.revisions.get(&key) {
            return if existing == &registration {
                Ok(ExecutableEnvironmentRegistrationOutcome::AlreadyRegistered)
            } else {
                Err(ExecutableEnvironmentRegistrationError::Conflict(format!(
                    "Environment `{id}` revision {} already has fingerprint `{}`",
                    revision.0, existing.fingerprint
                )))
            };
        }
        state.revisions.insert(key, registration.clone());
        let advances = state
            .current
            .get(&id)
            .is_none_or(|current| revision > current.lifecycle_revision);
        if advances {
            state.current.insert(
                id,
                CurrentEntry {
                    lifecycle_revision: revision,
                    registration: Some(registration),
                },
            );
            Ok(ExecutableEnvironmentRegistrationOutcome::RegisteredCurrent)
        } else {
            Ok(ExecutableEnvironmentRegistrationOutcome::RegisteredHistorical)
        }
    }

    fn withdraw_locked(
        state: &mut CatalogState,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        withdrawal.validate()?;
        match state.current.get_mut(&withdrawal.environment_id) {
            Some(current) if withdrawal.lifecycle_revision < current.lifecycle_revision => {
                Ok(ExecutableEnvironmentWithdrawalOutcome::HistoricalNoop)
            }
            Some(current)
                if withdrawal.lifecycle_revision == current.lifecycle_revision
                    && current.registration.is_none() =>
            {
                Ok(ExecutableEnvironmentWithdrawalOutcome::AlreadyWithdrawn)
            }
            Some(current) => {
                current.lifecycle_revision = withdrawal.lifecycle_revision;
                current.registration = None;
                Ok(ExecutableEnvironmentWithdrawalOutcome::WithdrawnCurrent)
            }
            None => {
                state.current.insert(
                    withdrawal.environment_id,
                    CurrentEntry {
                        lifecycle_revision: withdrawal.lifecycle_revision,
                        registration: None,
                    },
                );
                Ok(ExecutableEnvironmentWithdrawalOutcome::WithdrawnCurrent)
            }
        }
    }

    fn preview_registration(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let mut state = self
            .state
            .read()
            .expect("executable Environment catalog")
            .clone();
        Self::register_locked(&mut state, registration)
    }

    fn preview_withdrawal(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        let mut state = self
            .state
            .read()
            .expect("executable Environment catalog")
            .clone();
        Self::withdraw_locked(&mut state, withdrawal)
    }
}

#[async_trait]
impl ExecutableEnvironmentRegistrationSource for ExecutableEnvironmentCatalog {
    async fn current_registrations(
        &self,
    ) -> Result<Vec<ExecutableEnvironmentRegistration>, ExecutableEnvironmentRegistrationError>
    {
        Ok(self.current_all())
    }

    async fn current_registration(
        &self,
        environment_id: &str,
    ) -> Result<Option<ExecutableEnvironmentRegistration>, ExecutableEnvironmentRegistrationError>
    {
        Ok(self.current(environment_id))
    }

    async fn registration_at_revision(
        &self,
        environment_id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<ExecutableEnvironmentRegistration>, ExecutableEnvironmentRegistrationError>
    {
        Ok(self.at_revision(environment_id, revision))
    }
}

#[derive(Clone)]
pub struct LocalExecutableEnvironmentRegistrar {
    catalog: Arc<ExecutableEnvironmentCatalog>,
}

impl LocalExecutableEnvironmentRegistrar {
    #[must_use]
    pub fn new(catalog: Arc<ExecutableEnvironmentCatalog>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl ExecutableEnvironmentRegistrar for LocalExecutableEnvironmentRegistrar {
    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let mut state = self.catalog.state.write().map_err(|_| {
            ExecutableEnvironmentRegistrationError::Storage("catalog lock poisoned".into())
        })?;
        ExecutableEnvironmentCatalog::register_locked(&mut state, registration)
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        let mut state = self.catalog.state.write().map_err(|_| {
            ExecutableEnvironmentRegistrationError::Storage("catalog lock poisoned".into())
        })?;
        ExecutableEnvironmentCatalog::withdraw_locked(&mut state, withdrawal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_environment_contract::{EnvItem, EnvironmentConfig};

    fn registration(revision: u64, name: &str) -> ExecutableEnvironmentRegistration {
        ExecutableEnvironmentRegistration::new(
            EnvItem {
                id: "env-a".into(),
                revision: EnvironmentRevision(revision),
                name: name.into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
                archived_at: None,
            },
            None,
        )
    }

    #[tokio::test]
    async fn decision_table_preserves_exact_history_and_monotonic_current() {
        // Cause/effect design:
        // C1 first revision -> E1 becomes current;
        // C2 newer revision -> E2 becomes current and E1 stays exact-readable;
        // C3 replay same revision/facts -> E3 idempotent;
        // C4 same revision/different facts -> E4 conflict;
        // C5 withdrawal at current revision -> E5 current unavailable, history retained.
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let registrar = LocalExecutableEnvironmentRegistrar::new(catalog.clone());
        assert_eq!(
            registrar.register(registration(1, "one")).await.unwrap(),
            ExecutableEnvironmentRegistrationOutcome::RegisteredCurrent,
            "E1"
        );
        assert_eq!(
            registrar.register(registration(2, "two")).await.unwrap(),
            ExecutableEnvironmentRegistrationOutcome::RegisteredCurrent,
            "E2"
        );
        assert_eq!(
            catalog
                .at_revision("env-a", EnvironmentRevision(1))
                .unwrap()
                .definition
                .name,
            "one",
            "E2"
        );
        assert_eq!(
            registrar.register(registration(2, "two")).await.unwrap(),
            ExecutableEnvironmentRegistrationOutcome::AlreadyRegistered,
            "E3"
        );
        assert!(
            registrar.register(registration(2, "other")).await.is_err(),
            "E4"
        );
        registrar
            .withdraw(ExecutableEnvironmentWithdrawal {
                environment_id: "env-a".into(),
                lifecycle_revision: EnvironmentRevision(2),
            })
            .await
            .unwrap();
        assert!(catalog.current("env-a").is_none(), "E5");
        assert!(
            catalog
                .at_revision("env-a", EnvironmentRevision(2))
                .is_some(),
            "E5"
        );
    }
}
