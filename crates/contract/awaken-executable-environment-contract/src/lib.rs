//! Control-to-Coordinator executable Environment registration boundary.
//!
//! Control owns authored definitions and policy versions. Coordinator stores a
//! rebuildable projection used for Session admission and work coordination. The
//! registration contains the exact immutable facts; it is not a second authoring
//! model and it carries no WorkQueue state.

use async_trait::async_trait;
use awaken_environment_contract::{EnvItem, EnvironmentRevision};
use awaken_provisioning_contract::SandboxExecutionPolicy;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableEnvironmentRegistration {
    pub definition: EnvItem,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<SandboxExecutionPolicy>,
    pub fingerprint: String,
}

impl ExecutableEnvironmentRegistration {
    #[must_use]
    pub fn new(definition: EnvItem, sandbox_policy: Option<SandboxExecutionPolicy>) -> Self {
        let fingerprint = awaken_environment_contract::environment_facts_fingerprint(&(
            &definition,
            &sandbox_policy,
        ));
        Self {
            definition,
            sandbox_policy,
            fingerprint,
        }
    }

    pub fn validate(&self) -> Result<(), ExecutableEnvironmentRegistrationError> {
        if self.definition.id.trim().is_empty() {
            return Err(ExecutableEnvironmentRegistrationError::Invalid(
                "environment_id must not be empty".into(),
            ));
        }
        if self.definition.revision.0 == 0 {
            return Err(ExecutableEnvironmentRegistrationError::Invalid(
                "revision must be non-zero".into(),
            ));
        }
        if self.definition.archived_at.is_some() {
            return Err(ExecutableEnvironmentRegistrationError::Invalid(
                "archived definitions must be withdrawn, not registered".into(),
            ));
        }
        let expected = Self::new(self.definition.clone(), self.sandbox_policy.clone());
        if self.fingerprint.is_empty() || self.fingerprint != expected.fingerprint {
            return Err(ExecutableEnvironmentRegistrationError::Invalid(
                "registration fingerprint does not match its immutable facts".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableEnvironmentWithdrawal {
    pub environment_id: String,
    pub lifecycle_revision: EnvironmentRevision,
}

impl ExecutableEnvironmentWithdrawal {
    pub fn validate(&self) -> Result<(), ExecutableEnvironmentRegistrationError> {
        if self.environment_id.trim().is_empty() || self.lifecycle_revision.0 == 0 {
            return Err(ExecutableEnvironmentRegistrationError::Invalid(
                "withdrawal requires a non-empty id and non-zero revision".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutableEnvironmentRegistrationOutcome {
    RegisteredCurrent,
    RegisteredHistorical,
    AlreadyRegistered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutableEnvironmentWithdrawalOutcome {
    WithdrawnCurrent,
    AlreadyWithdrawn,
    HistoricalNoop,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum ExecutableEnvironmentRegistrationError {
    #[error("invalid executable Environment registration: {0}")]
    Invalid(String),
    #[error("conflicting executable Environment registration: {0}")]
    Conflict(String),
    #[error("executable Environment registration unavailable: {0}")]
    Unavailable(String),
    #[error("executable Environment registration storage failure: {0}")]
    Storage(String),
}

#[async_trait]
pub trait ExecutableEnvironmentRegistrar: Send + Sync {
    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>;

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>;
}

#[async_trait]
pub trait ExecutableEnvironmentRegistrationSource: Send + Sync {
    async fn current_registration(
        &self,
        environment_id: &str,
    ) -> Result<Option<ExecutableEnvironmentRegistration>, ExecutableEnvironmentRegistrationError>;

    async fn registration_at_revision(
        &self,
        environment_id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<ExecutableEnvironmentRegistration>, ExecutableEnvironmentRegistrationError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_environment_contract::EnvironmentConfig;

    fn definition() -> EnvItem {
        EnvItem {
            id: "env-a".into(),
            revision: EnvironmentRevision(2),
            name: "A".into(),
            description: String::new(),
            metadata: Default::default(),
            scope: None,
            config: EnvironmentConfig::SelfHosted,
            sandbox_policy: None,
            archived_at: None,
        }
    }

    #[test]
    fn decision_table_validates_exact_registration_identity() {
        // Cause/effect design:
        // C1 exact facts + derived fingerprint -> E1 valid;
        // C2 any fact changes without recomputing the fingerprint -> E2 invalid;
        // C3 an archived definition, even with a matching fingerprint -> E3
        // invalid because lifecycle tombstones use the withdrawal command.
        //
        // | Rule | fingerprint | archived | effect |
        // | R1 | exact | false | valid registration |
        // | R2 | stale | false | invalid registration |
        // | R3 | exact | true | invalid; withdrawal required |
        let valid = ExecutableEnvironmentRegistration::new(definition(), None);
        assert!(valid.validate().is_ok(), "R1");
        let mut changed = valid.clone();
        changed.definition.name = "changed".into();
        assert!(changed.validate().is_err(), "R2");
        let mut archived = valid.definition;
        archived.archived_at = Some("2026-01-01T00:00:00Z".into());
        assert!(
            ExecutableEnvironmentRegistration::new(archived, None)
                .validate()
                .is_err(),
            "R3"
        );
    }
}
