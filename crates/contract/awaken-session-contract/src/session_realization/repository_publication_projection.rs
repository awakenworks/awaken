//! Closed Session-root projection for one terminal Repository publication.
//!
//! The projection stays under Session realization Control ownership: this
//! module only owns its canonical wire and structural validation, never a
//! second publication command, lease, or receipt authority.

use super::SessionRealizationControlFailure;
use crate::SessionRealizationLease;

/// One aggregate-derived terminal Repository publication together with the
/// Session row's immutable owning Workspace. Coordinator adapters project both
/// facts through the same Control read so an authenticated Worker cannot choose
/// a different tenant coordinate for the Repository transport hop.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRepositoryPublicationProjection {
    pub workspace_id: String,
    pub command: crate::SessionRepositoryPublicationCommand,
    /// Exact current Session-root realization read with the command. The
    /// caller's asserted lease may have expired; this monotonic same-generation
    /// readback is the only temporal authority for starting the publication.
    pub current_lease: SessionRealizationLease,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionRepositoryPublicationProjectionWire {
    workspace_id: String,
    command: crate::SessionRepositoryPublicationCommand,
    current_lease: SessionRepositoryPublicationLeaseWire,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionRepositoryPublicationLeaseWire {
    owner: String,
    runtime_incarnation: String,
    epoch: u64,
    expires_at_unix_ms: u64,
}

impl From<SessionRepositoryPublicationLeaseWire> for SessionRealizationLease {
    fn from(wire: SessionRepositoryPublicationLeaseWire) -> Self {
        Self {
            owner: wire.owner,
            runtime_incarnation: wire.runtime_incarnation,
            epoch: wire.epoch,
            expires_at_unix_ms: wire.expires_at_unix_ms,
        }
    }
}

impl<'de> serde::Deserialize<'de> for SessionRepositoryPublicationProjection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = <SessionRepositoryPublicationProjectionWire as serde::Deserialize>::deserialize(
            deserializer,
        )?;
        Self::try_new(wire.workspace_id, wire.command, wire.current_lease.into())
            .map_err(serde::de::Error::custom)
    }
}

impl SessionRepositoryPublicationProjection {
    /// Close one canonical publication readback over the immutable command,
    /// Workspace owner, and current aggregate lease. Wall-clock liveness stays
    /// at the Session Control boundary; wire validation rejects structurally
    /// unusable coordinates without creating another clock owner.
    pub fn try_new(
        workspace_id: String,
        command: crate::SessionRepositoryPublicationCommand,
        current_lease: SessionRealizationLease,
    ) -> Result<Self, SessionRealizationControlFailure> {
        if workspace_id.trim().is_empty()
            || command.session_id.trim().is_empty()
            || current_lease.expires_at_unix_ms == 0
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "Session Repository publication projection is incomplete".into(),
            ));
        }
        current_lease
            .sandbox_effect_fence(command.effect_id.clone())
            .map_err(|error| {
                SessionRealizationControlFailure::Invalid(format!(
                    "Session Repository publication projection has an invalid lease: {error}"
                ))
            })?;
        command.intent.validate().map_err(|error| {
            SessionRealizationControlFailure::Invalid(format!(
                "Session Repository publication projection is invalid: {error}"
            ))
        })?;
        Ok(Self {
            workspace_id,
            command,
            current_lease,
        })
    }
}
