// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: a rebuildable Coordinator projection of the Registry's process-end receipt
// @formal reason: for the Worker incarnation that materialized a Resident source. Dynamic path: the
// @formal reason: Coordinator reads the closed incarnation's receipt and the Worker uses it only for the
// @formal reason: handle fenced by that incarnation; absence keeps a Rebuild pending. No authority moves.
//! The recorded end of the Worker incarnation a Resident source is bound to.
//!
//! A source realized in a Worker's own processes and filesystem (the Workdir
//! and Namespace tiers) is closed only once that Worker incarnation's
//! processes have ended. The Registry owns that fact (its process-exit
//! receipt), and a Worker has no Registry read. The Coordinator therefore
//! projects the fact onto the realization directive it already sends
//! ([`crate::FrozenSessionProjection::bound_source_end`]), and the Worker's
//! realization reads it only for the exact handle whose frozen effect fence
//! names that incarnation.
//!
//! Coordinator table ([`BoundSourceEnd::bound_incarnation`] and
//! [`BoundSourceEnd::from_registry`]):
//!
//! | Rule | Session Environment                                                 | Registry receipt of that incarnation | Projected |
//! |------|---------------------------------------------------------------------|--------------------------------------|-----------|
//! | BE1  | not Resident, or Resident without an effect id                      | not read                             | none      |
//! | BE2  | Resident, its effect is not a recorded Create/Rebuild intent        | not read                             | none      |
//! | BE3  | Resident by a recorded Create/Rebuild intent without a lease        | not read                             | none      |
//! | BE4  | Resident by a recorded intent under lease L                         | absent (live or unrecorded)          | none      |
//! | BE5  | same                                                                | exact exit                           | ExactExit |
//! | BE6  | same                                                                | superseded                           | Superseded|
//!
//! A binding re-proved by an Adopt carries an effect id no recorded intent
//! has (BE2), so a source another incarnation may have adopted is never
//! reported closed from its creator's end.
//!
//! Worker table ([`BoundSourceEnd::end_for`]):
//!
//! | Rule | Handle's frozen effect fence             | Result   |
//! |------|------------------------------------------|----------|
//! | BW1  | absent (legacy or provider-owned handle) | none     |
//! | BW2  | another owner or Runtime incarnation     | none     |
//! | BW3  | the projected owner and incarnation      | its end  |
//!
//! "None" leaves the source's incarnation end unrecorded, which keeps a
//! process-bound Rebuild pending: the fail-closed direction.

use awaken_session_contract::{SessionEnvironmentMaterialization, SessionEnvironmentState};
use awaken_worker_contract::{WorkerProcessEnd, WorkerProcessExit};

/// How the bound incarnation's processes are known to have ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedProcessEnd {
    /// The provider reported every bound process's exact exit.
    ExactExit,
    /// A newer instance of the bound process slot started (Kubernetes: a newer
    /// container of the same name in the same Pod UID); no exit code.
    Superseded,
}

/// The Registry-recorded end of the Worker incarnation that holds the
/// Session's Resident source, named by its Session realization lease owner.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundSourceEnd {
    pub owner: String,
    pub runtime_incarnation: String,
    pub end: RecordedProcessEnd,
}

impl BoundSourceEnd {
    /// The lease (owner, Runtime incarnation) under which the Resident binding
    /// was last materialized by a recorded Create or Rebuild intent (rules
    /// BE1–BE3). Only that incarnation can run the source's processes.
    #[must_use]
    pub fn bound_incarnation(
        environment: &SessionEnvironmentState,
        materialization: Option<&SessionEnvironmentMaterialization>,
    ) -> Option<(String, String)> {
        let SessionEnvironmentState::Resident {
            effect_id: Some(effect_id),
            ..
        } = environment
        else {
            return None;
        };
        let materialization = materialization?;
        let intent = [materialization.origin(), materialization.pending()]
            .into_iter()
            .flatten()
            .find(|intent| intent.effect_id() == effect_id)?;
        if !matches!(
            intent.kind(),
            awaken_session_contract::SessionEnvironmentEffectKind::Create
                | awaken_session_contract::SessionEnvironmentEffectKind::Rebuild { .. }
        ) {
            return None;
        }
        let lease = intent.realization()?;
        Some((lease.owner.clone(), lease.runtime_incarnation.clone()))
    }

    /// Project the Registry's receipt for that incarnation (rules BE4–BE6).
    /// The caller reads it with `read_closed_incarnation_exit`, which already
    /// refuses a live, foreign or malformed record.
    #[must_use]
    pub fn from_registry(
        owner: String,
        runtime_incarnation: String,
        receipt: Option<&WorkerProcessExit>,
    ) -> Option<Self> {
        let end = match receipt?.end() {
            WorkerProcessEnd::ExactExit => RecordedProcessEnd::ExactExit,
            WorkerProcessEnd::Superseded => RecordedProcessEnd::Superseded,
        };
        Some(Self {
            owner,
            runtime_incarnation,
            end,
        })
    }

    /// The Coordinator's read: the Session's materializing incarnation
    /// (BE1–BE3), unless it is the current lease's own live incarnation, and
    /// its closed-incarnation receipt (BE4–BE6). Read failures propagate.
    pub async fn read(
        session: &awaken_session_contract::PersistedSession,
        workers: &dyn awaken_worker_contract::WorkerIncarnationSource,
    ) -> Result<Option<Self>, awaken_worker_contract::RegistryError> {
        let Some((owner, runtime_incarnation)) =
            Self::bound_incarnation(&session.environment, session.environment_materialization())
        else {
            return Ok(None);
        };
        if session.realization.as_ref().is_some_and(|lease| {
            lease.owner == owner && lease.runtime_incarnation == runtime_incarnation
        }) {
            return Ok(None);
        }
        let Some(identity) =
            awaken_worker_contract::WorkerIdentity::from_lease_owner(&owner, &runtime_incarnation)
        else {
            return Ok(None);
        };
        let receipt =
            awaken_worker_contract::read_closed_incarnation_exit(workers, &identity).await?;
        Ok(Self::from_registry(
            owner,
            runtime_incarnation,
            receipt.as_ref(),
        ))
    }

    /// The end for exactly this handle (rules BW1–BW3): only a handle whose
    /// frozen effect fence names the projected owner and incarnation.
    #[must_use]
    pub fn end_for(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Option<RecordedProcessEnd> {
        let fence = handle
            .evidence()
            .and_then(awaken_provisioning_contract::HandleEvidence::effect_fence)?;
        (fence.owner() == self.owner && fence.runtime_incarnation() == self.runtime_incarnation)
            .then_some(self.end)
    }
}

#[cfg(test)]
#[path = "bound_source_end/tests.rs"]
mod tests;
