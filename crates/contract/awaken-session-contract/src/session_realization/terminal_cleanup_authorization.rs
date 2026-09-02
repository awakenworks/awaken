//! Closed terminal assignment, work, and preparation authorization.
//!
//! This private module is a mechanical responsibility boundary. The Session
//! root remains the sole lifecycle authority, and the parent module re-exports
//! the unchanged public contract consumed by Control and Runtime adapters.

use super::{FrozenSessionProjection, SessionRealizationControlFailure, SessionRealizationLease};

/// One cold Worker assignment for an already-fenced terminal Session.
///
/// Cleanup commands deliberately do not travel in this assignment. The Worker
/// installs the frozen projection and then polls the parent Control's terminal
/// work and root Repository-publication projections, keeping
/// [`crate::SessionCleanupOperation`] as the only durable work queue and
/// completion registry.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionTerminalCleanupAssignment {
    pub session_id: String,
    pub projection: FrozenSessionProjection,
    pub lease: SessionRealizationLease,
}

/// One read-only projection of terminal work from a single Session-root
/// snapshot. `assignment` carries that snapshot's current realization lease and
/// frozen Environment/Resource facts; `action` is the mutually-exclusive next
/// step derived by [`crate::PersistedSession`]. The cleanup operation owns
/// progress, while that same aggregate joins an existing continuation disposal
/// predecessor from its Environment when required. This value owns no queue,
/// phase, receipt, or retry state beside that root.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionTerminalCleanupWork {
    pub assignment: SessionTerminalCleanupAssignment,
    pub action: crate::SessionTerminalCleanupAction,
}

/// Aggregate-derived authorization for one source-dependent terminal
/// preparation effect.
///
/// The exact effect is echoed privately so a Workspace-scoped authorization or
/// an inherited continuation predecessor cannot be replayed for another child,
/// root, or realization generation. `inherited_provider_disposal` is present
/// only when the root Environment has already durably crossed a continuation
/// preparation gate; ordinary terminal roots and every child receive `None`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTerminalCleanupPreparationAuthorization {
    effect: crate::SessionTerminalCleanupEffect,
    workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inherited_provider_disposal: Option<awaken_provisioning_contract::SandboxDisposalPreparation>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionTerminalCleanupPreparationAuthorizationWire {
    effect: crate::SessionTerminalCleanupEffect,
    workspace_id: String,
    #[serde(default)]
    inherited_provider_disposal: Option<awaken_provisioning_contract::SandboxDisposalPreparation>,
}

impl<'de> serde::Deserialize<'de> for SessionTerminalCleanupPreparationAuthorization {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SessionTerminalCleanupPreparationAuthorizationWire::deserialize(deserializer)?;
        Self::try_new(
            wire.effect,
            wire.workspace_id,
            wire.inherited_provider_disposal,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl SessionTerminalCleanupPreparationAuthorization {
    /// Close one root-authorized projection over its exact terminal effect.
    pub fn try_new(
        effect: crate::SessionTerminalCleanupEffect,
        workspace_id: String,
        inherited_provider_disposal: Option<
            awaken_provisioning_contract::SandboxDisposalPreparation,
        >,
    ) -> Result<Self, SessionRealizationControlFailure> {
        let authorization = Self {
            effect,
            workspace_id,
            inherited_provider_disposal,
        };
        authorization.validate()?;
        Ok(authorization)
    }

    fn validate(&self) -> Result<(), SessionRealizationControlFailure> {
        if self.workspace_id.trim().is_empty() {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal cleanup preparation authorization has no Workspace".into(),
            ));
        }
        let current = self
            .effect
            .sandbox_effect_fence()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        if let Some(inherited) = &self.inherited_provider_disposal {
            if self.effect.command.thread_id != self.effect.command.session_id {
                return Err(SessionRealizationControlFailure::Invalid(
                    "child terminal cleanup cannot inherit a root provider preparation".into(),
                ));
            }
            inherited
                .operation_id()
                .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
            if !inherited
                .prepared_effect_fence()
                .authorizes_successor(&current)
            {
                return Err(SessionRealizationControlFailure::Invalid(
                    "terminal cleanup preparation does not succeed its inherited provider fence"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Recheck that a transported authorization still names the exact effect
    /// being executed at the Runtime boundary.
    pub fn verify_for(
        &self,
        effect: &crate::SessionTerminalCleanupEffect,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.validate()?;
        if &self.effect != effect {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal cleanup preparation authorization names another effect".into(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    #[must_use]
    pub fn inherited_provider_disposal(
        &self,
    ) -> Option<&awaken_provisioning_contract::SandboxDisposalPreparation> {
        self.inherited_provider_disposal.as_ref()
    }
}
