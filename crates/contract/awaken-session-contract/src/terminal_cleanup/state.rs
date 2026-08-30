//! The single durable terminal-cleanup state machine.
//!
//! Preparation and publication modules contribute evidence and sidecars to
//! this operation, but neither owns a parallel phase hierarchy.

use super::effects::{cleanup_effect_id, complete_cleanup};
use super::{
    SessionCleanupCommand, SessionCleanupCompletion, SessionCleanupDisposing, SessionCleanupError,
    SessionCleanupPreparing, SessionRepositoryPublicationCleanup,
    SessionRepositoryPublicationCommand, SessionRepositoryPublicationIntent,
    VerifiedSessionCleanupReceipt, verified_repository_publication_outcome,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Heap-free phase projection used by the production operation gate and Kani.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SessionCleanupPhase {
    NotRequested,
    Fenced,
    Requested,
    Completed,
}

/// A durable cleanup operation may advance by exactly one phase. Replays are
/// handled by the phase-specific methods without rewriting durable authority.
#[must_use]
pub(crate) const fn session_cleanup_phase_advance_admitted(
    current: SessionCleanupPhase,
    next: SessionCleanupPhase,
) -> bool {
    matches!(
        (current, next),
        (
            SessionCleanupPhase::NotRequested,
            SessionCleanupPhase::Fenced
        ) | (SessionCleanupPhase::Fenced, SessionCleanupPhase::Requested)
            | (
                SessionCleanupPhase::Requested,
                SessionCleanupPhase::Completed
            )
    )
}

/// Private persisted representation of the one cleanup operation.
///
/// Keeping the tagged enum private is load-bearing: historical `Requested`
/// completions remain decodable only through the complete persisted Session
/// codec, while downstream crates cannot construct or mutate that legacy
/// one-stage evidence through Rust variants or direct serde decoding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum SessionCleanupState {
    #[default]
    NotRequested,
    /// Durable admission fence. No new Session Run or delegation may begin,
    /// but the currently executing parent has not yet reached quiescence.
    Fenced { effect_id: String },
    Requested {
        effect_id: String,
        #[serde(default)]
        thread_ids: BTreeSet<String>,
        #[serde(default)]
        delegation_watermark: u64,
        /// Runtime commit high-water captured after the terminal fence joined
        /// every admitted root/child execution.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_commit_cursor: Option<u64>,
        /// Canonical Runtime completions admitted by historical one-stage
        /// writers for this frozen target set. Current writers use the
        /// Preparing/Disposing states; this field remains the decode and
        /// readback authority for already-durable legacy evidence.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        completions: BTreeMap<String, SessionCleanupCompletion>,
    },
    Completed {
        effect_id: String,
        #[serde(default)]
        thread_ids: BTreeSet<String>,
        #[serde(default)]
        delegation_watermark: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_commit_cursor: Option<u64>,
        receipt_fingerprint: String,
    },
    /// Additive two-stage cleanup progress around one unchanged Requested
    /// operation. The first new-style preparation atomically enters this
    /// boxed variant, so older readers reject the unknown state instead of
    /// mistaking preparation for the legacy physical-completion evidence.
    Preparing(Box<SessionCleanupPreparing>),
    /// Every frozen target has durable preparation evidence. Only this boxed
    /// variant may project the one root-owned physical disposal command.
    Disposing(Box<SessionCleanupDisposing>),
    /// Additive publication metadata around exactly one non-publication cleanup
    /// operation. The private payload and custom decoder reject recursive
    /// wrappers, so this cannot become a parallel phase hierarchy.
    RepositoryPublication(Box<SessionRepositoryPublicationCleanup>),
}

/// The one durable cleanup operation stored by the Session aggregate.
///
/// This public value is intentionally opaque. Current callers may request and
/// drive the canonical two-stage transition only through its methods. The
/// private persisted state retains the historical wire grammar without
/// retaining a public legacy-completion authoring surface.
///
/// Historical variants are not a public construction or pattern-matching API:
///
/// ```compile_fail
/// use awaken_session_contract::SessionCleanupOperation;
/// let _ = SessionCleanupOperation::NotRequested;
/// ```
///
/// Historical wire decoding belongs to the complete persisted Session codec,
/// not to this public operation value:
///
/// ```compile_fail
/// use awaken_session_contract::SessionCleanupOperation;
/// fn requires_deserialize<T: for<'de> serde::Deserialize<'de>>() {}
/// requires_deserialize::<SessionCleanupOperation>();
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SessionCleanupOperation {
    pub(super) state: SessionCleanupState,
}

/// Decode the private historical wire only when it is embedded in the complete
/// persisted Session aggregate (or another private cleanup wrapper). There is
/// deliberately no public `Deserialize` implementation for
/// [`SessionCleanupOperation`].
pub(crate) fn deserialize_persisted_operation<'de, D>(
    deserializer: D,
) -> Result<SessionCleanupOperation, D::Error>
where
    D: serde::Deserializer<'de>,
{
    SessionCleanupState::deserialize(deserializer).map(|state| SessionCleanupOperation { state })
}

impl SessionCleanupOperation {
    pub(super) const fn from_state(state: SessionCleanupState) -> Self {
        Self { state }
    }

    pub(super) const fn state(&self) -> &SessionCleanupState {
        &self.state
    }

    pub(super) fn state_mut(&mut self) -> &mut SessionCleanupState {
        &mut self.state
    }

    #[must_use]
    pub(crate) fn phase(&self) -> SessionCleanupPhase {
        match self.legacy_cleanup().state() {
            SessionCleanupState::NotRequested => SessionCleanupPhase::NotRequested,
            SessionCleanupState::Fenced { .. } => SessionCleanupPhase::Fenced,
            SessionCleanupState::Requested { .. } => SessionCleanupPhase::Requested,
            SessionCleanupState::Completed { .. } => SessionCleanupPhase::Completed,
            SessionCleanupState::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
            SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_) => {
                unreachable!("cleanup progress wrappers cannot be nested")
            }
        }
    }

    pub(super) fn progress_inner(&self) -> &Self {
        match self.state() {
            SessionCleanupState::Preparing(preparing) => &preparing.progress.cleanup,
            SessionCleanupState::Disposing(disposing) => &disposing.progress.cleanup,
            _ => self,
        }
    }

    pub(super) fn progress_inner_mut(&mut self) -> &mut Self {
        match self.state() {
            SessionCleanupState::Preparing(_) => {
                let SessionCleanupState::Preparing(preparing) = self.state_mut() else {
                    unreachable!()
                };
                &mut preparing.progress.cleanup
            }
            SessionCleanupState::Disposing(_) => {
                let SessionCleanupState::Disposing(disposing) = self.state_mut() else {
                    unreachable!()
                };
                &mut disposing.progress.cleanup
            }
            _ => self,
        }
    }

    pub(super) fn legacy_cleanup(&self) -> &Self {
        let cleanup = self.progress_inner();
        match cleanup.state() {
            SessionCleanupState::RepositoryPublication(publication) => &publication.cleanup,
            _ => cleanup,
        }
    }

    pub(super) fn effect_id(&self) -> Option<&str> {
        match self.legacy_cleanup().state() {
            SessionCleanupState::Fenced { effect_id }
            | SessionCleanupState::Requested { effect_id, .. }
            | SessionCleanupState::Completed { effect_id, .. } => Some(effect_id),
            SessionCleanupState::NotRequested => None,
            SessionCleanupState::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
            SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_) => {
                unreachable!("cleanup progress wrappers cannot be nested")
            }
        }
    }

    pub(super) fn advance_to(&mut self, next: Self) -> bool {
        if matches!(
            self.state(),
            SessionCleanupState::RepositoryPublication(_)
                | SessionCleanupState::Preparing(_)
                | SessionCleanupState::Disposing(_)
        ) || matches!(
            next.state(),
            SessionCleanupState::RepositoryPublication(_)
                | SessionCleanupState::Preparing(_)
                | SessionCleanupState::Disposing(_)
        ) {
            return false;
        }
        if !session_cleanup_phase_advance_admitted(self.phase(), next.phase()) {
            return false;
        }
        *self = next;
        true
    }

    /// Commit the stable whole-Session operation before any Runtime effect.
    pub fn request(&mut self, session_id: &str) -> bool {
        self.advance_to(Self::from_state(SessionCleanupState::Fenced {
            effect_id: cleanup_effect_id(session_id),
        }))
    }

    /// Commit an explicit Repository publication intent in the same terminal
    /// fence that owns every later cleanup effect. Exact retries are no-ops;
    /// another or absent publication intent cannot rewrite frozen authority.
    pub fn request_with_publication(
        &mut self,
        session_id: &str,
        repository_publication: SessionRepositoryPublicationIntent,
    ) -> Result<bool, SessionCleanupError> {
        repository_publication.validate()?;
        if matches!(
            self.state(),
            SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_)
        ) {
            return if self.effect_id() == Some(cleanup_effect_id(session_id).as_str())
                && self.repository_publication_intent() == Some(&repository_publication)
            {
                Ok(false)
            } else {
                Err(SessionCleanupError::FrozenRepositoryPublicationMismatch)
            };
        }
        match self.state() {
            SessionCleanupState::NotRequested => {
                let mut cleanup = Self::default();
                if !cleanup.request(session_id) {
                    return Err(SessionCleanupError::InvalidPhaseAdvance);
                }
                *self = Self::from_state(SessionCleanupState::RepositoryPublication(Box::new(
                    SessionRepositoryPublicationCleanup {
                        cleanup,
                        intent: repository_publication,
                        receipt: None,
                        rejection: None,
                    },
                )));
                Ok(true)
            }
            SessionCleanupState::RepositoryPublication(publication) => {
                if publication.cleanup.effect_id() == Some(cleanup_effect_id(session_id).as_str())
                    && publication.intent == repository_publication
                {
                    Ok(false)
                } else {
                    Err(SessionCleanupError::FrozenRepositoryPublicationMismatch)
                }
            }
            SessionCleanupState::Fenced { .. }
            | SessionCleanupState::Requested { .. }
            | SessionCleanupState::Completed { .. } => {
                Err(SessionCleanupError::FrozenRepositoryPublicationMismatch)
            }
            SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_) => {
                unreachable!()
            }
        }
    }

    /// Freeze the complete target set only after the terminal fence is durable
    /// and the parent Runtime has stopped. A Requested/Completed intent is
    /// immutable: recovery may replay it, but no later projection can expand it.
    pub fn freeze_targets(
        &mut self,
        session_id: &str,
        thread_ids: impl IntoIterator<Item = String>,
        delegation_watermark: u64,
        runtime_commit_cursor: u64,
    ) -> Result<bool, SessionCleanupError> {
        if matches!(
            self.state(),
            SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_)
        ) {
            return self.progress_inner_mut().freeze_targets(
                session_id,
                thread_ids,
                delegation_watermark,
                runtime_commit_cursor,
            );
        }
        if let SessionCleanupState::RepositoryPublication(publication) = self.state_mut() {
            return publication.cleanup.freeze_targets(
                session_id,
                thread_ids,
                delegation_watermark,
                runtime_commit_cursor,
            );
        }
        let SessionCleanupState::Fenced { effect_id } = self.state() else {
            return match self.state() {
                SessionCleanupState::Requested {
                    thread_ids: durable,
                    delegation_watermark: durable_watermark,
                    runtime_commit_cursor: durable_cursor,
                    ..
                }
                | SessionCleanupState::Completed {
                    thread_ids: durable,
                    delegation_watermark: durable_watermark,
                    runtime_commit_cursor: durable_cursor,
                    ..
                } => {
                    let mut asserted = BTreeSet::from([session_id.to_string()]);
                    asserted.extend(thread_ids);
                    if *durable == asserted
                        && *durable_watermark == delegation_watermark
                        && *durable_cursor == Some(runtime_commit_cursor)
                    {
                        Ok(false)
                    } else {
                        Err(SessionCleanupError::FrozenTargetsMismatch)
                    }
                }
                SessionCleanupState::NotRequested => Err(SessionCleanupError::NotRequested),
                SessionCleanupState::Fenced { .. } => unreachable!(),
                SessionCleanupState::RepositoryPublication(_) => unreachable!(),
                SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_) => {
                    unreachable!()
                }
            };
        };
        if *effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        let effect_id = effect_id.clone();
        let mut durable = BTreeSet::from([session_id.to_string()]);
        durable.extend(thread_ids);
        let advanced = self.advance_to(Self::from_state(SessionCleanupState::Requested {
            effect_id,
            thread_ids: durable,
            delegation_watermark,
            runtime_commit_cursor: Some(runtime_commit_cursor),
            completions: BTreeMap::new(),
        }));
        if !advanced {
            return Err(SessionCleanupError::InvalidPhaseAdvance);
        }
        Ok(advanced)
    }

    #[must_use]
    pub fn thread_ids(&self) -> Option<&BTreeSet<String>> {
        match self.legacy_cleanup().state() {
            SessionCleanupState::Requested { thread_ids, .. }
            | SessionCleanupState::Completed { thread_ids, .. } => Some(thread_ids),
            SessionCleanupState::NotRequested | SessionCleanupState::Fenced { .. } => None,
            SessionCleanupState::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
            SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_) => {
                unreachable!("cleanup progress wrappers cannot be nested")
            }
        }
    }

    /// Frozen delegation high-water for Requested/Completed cleanup.
    #[must_use]
    pub fn delegation_watermark(&self) -> Option<u64> {
        match self.legacy_cleanup().state() {
            SessionCleanupState::Requested {
                delegation_watermark,
                ..
            }
            | SessionCleanupState::Completed {
                delegation_watermark,
                ..
            } => Some(*delegation_watermark),
            SessionCleanupState::NotRequested | SessionCleanupState::Fenced { .. } => None,
            SessionCleanupState::RepositoryPublication(_)
            | SessionCleanupState::Preparing(_)
            | SessionCleanupState::Disposing(_) => {
                unreachable!("cleanup wrappers are removed by legacy_cleanup")
            }
        }
    }

    /// Immutable terminal projection boundary after Runtime quiescence.
    #[must_use]
    pub fn runtime_commit_cursor(&self) -> Option<u64> {
        match self.legacy_cleanup().state() {
            SessionCleanupState::Requested {
                runtime_commit_cursor,
                ..
            }
            | SessionCleanupState::Completed {
                runtime_commit_cursor,
                ..
            } => *runtime_commit_cursor,
            SessionCleanupState::NotRequested | SessionCleanupState::Fenced { .. } => None,
            SessionCleanupState::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
            SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_) => {
                unreachable!("cleanup progress wrappers cannot be nested")
            }
        }
    }

    #[must_use]
    pub fn is_fenced(&self) -> bool {
        matches!(
            self.legacy_cleanup().state(),
            SessionCleanupState::Fenced { .. }
        )
    }

    #[must_use]
    pub fn is_not_requested(&self) -> bool {
        matches!(
            self.legacy_cleanup().state(),
            SessionCleanupState::NotRequested
        )
    }

    #[must_use]
    pub fn is_requested(&self) -> bool {
        matches!(
            self.legacy_cleanup().state(),
            SessionCleanupState::Requested { .. }
        )
    }

    #[must_use]
    pub fn is_completed(&self) -> bool {
        matches!(
            self.legacy_cleanup().state(),
            SessionCleanupState::Completed { .. }
        )
    }

    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        matches!(
            self.legacy_cleanup().state(),
            SessionCleanupState::Fenced { .. } | SessionCleanupState::Requested { .. }
        )
    }

    #[must_use]
    pub fn command_for(&self, session_id: &str, thread_id: &str) -> Option<SessionCleanupCommand> {
        let cleanup = self.legacy_cleanup();
        let (effect_id, thread_ids) = match cleanup.state() {
            SessionCleanupState::Requested {
                effect_id,
                thread_ids,
                ..
            }
            | SessionCleanupState::Completed {
                effect_id,
                thread_ids,
                ..
            } => (effect_id, thread_ids),
            SessionCleanupState::NotRequested | SessionCleanupState::Fenced { .. } => return None,
            SessionCleanupState::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
            SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_) => {
                unreachable!("cleanup progress wrappers cannot be nested")
            }
        };
        if !thread_ids.contains(thread_id) {
            return None;
        }
        Some(SessionCleanupCommand::new(session_id, thread_id, effect_id))
    }

    /// Commands in the immutable target set that have no verified completion
    /// yet. This is the sole durable remote-work projection: callers may poll it,
    /// but cannot add targets or author another cleanup registry.
    #[cfg(test)]
    pub(crate) fn pending_commands(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionCleanupCommand>, SessionCleanupError> {
        if matches!(
            self.state(),
            SessionCleanupState::Preparing(_) | SessionCleanupState::Disposing(_)
        ) {
            // New-style progress is projected only through the preparation and
            // disposal APIs. A downgraded combined-effect caller therefore
            // cannot cross the new physical-deletion barrier.
            return Ok(Vec::new());
        }
        let cleanup = self.legacy_cleanup();
        let SessionCleanupState::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = cleanup.state()
        else {
            return if cleanup.is_completed() {
                Ok(Vec::new())
            } else {
                Err(SessionCleanupError::NotRequested)
            };
        };
        if *effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        let children = thread_ids
            .iter()
            .filter(|thread_id| {
                thread_id.as_str() != session_id && !completions.contains_key(*thread_id)
            })
            .map(|thread_id| SessionCleanupCommand::new(session_id, thread_id, effect_id))
            .collect::<Vec<_>>();
        if !children.is_empty() {
            // The root command disposes the shared Worker projection. Keep it
            // behind every child receipt so a failed child remains retryable on
            // the same realization owner instead of losing its poll cursor.
            return Ok(children);
        }
        if self.publication_command(session_id)?.is_some() {
            // Repository publication is a root-owned effect, but the ordinary
            // root cleanup command would dispose its working tree. Withhold that
            // disposer until the exact publication receipt is durable.
            return Ok(Vec::new());
        }
        Ok(thread_ids
            .contains(session_id)
            .then(|| {
                (!completions.contains_key(session_id))
                    .then(|| SessionCleanupCommand::new(session_id, session_id, effect_id))
            })
            .flatten()
            .into_iter()
            .collect())
    }

    /// Re-verify every durable remote completion against the immutable command
    /// set before it can become aggregate completion evidence.
    pub(super) fn recorded_receipts(
        &self,
        session_id: &str,
    ) -> Result<Vec<VerifiedSessionCleanupReceipt>, SessionCleanupError> {
        let cleanup = self.legacy_cleanup();
        let SessionCleanupState::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = cleanup.state()
        else {
            return Err(SessionCleanupError::NotRequested);
        };
        if completions.len() != thread_ids.len() {
            return Err(SessionCleanupError::MissingReceipt);
        }
        thread_ids
            .iter()
            .map(|thread_id| {
                let command = SessionCleanupCommand::new(session_id, thread_id, effect_id);
                completions
                    .get(thread_id)
                    .ok_or(SessionCleanupError::MissingReceipt)?
                    .verify(&command)
            })
            .collect()
    }

    /// Report whether a historical one-stage Requested row already contains
    /// the complete canonical receipt set. This is a read-only compatibility
    /// projection; current writers cannot add another completion through it.
    pub(crate) fn has_complete_legacy_receipts(
        &self,
        session_id: &str,
    ) -> Result<bool, SessionCleanupError> {
        match self.recorded_receipts(session_id) {
            Ok(_) => Ok(true),
            Err(SessionCleanupError::MissingReceipt) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Commit verified receipt evidence only after every thread effect has
    /// succeeded. Raw Runtime completions cannot cross this boundary.
    pub(super) fn complete(
        &mut self,
        session_id: &str,
        receipts: &[VerifiedSessionCleanupReceipt],
    ) -> Result<bool, SessionCleanupError> {
        if let SessionCleanupState::RepositoryPublication(publication) = self.state_mut() {
            let publication = publication.as_mut();
            let effect_id = publication
                .cleanup
                .effect_id()
                .ok_or(SessionCleanupError::NotRequested)?;
            let command = SessionRepositoryPublicationCommand::new(
                session_id,
                effect_id,
                &publication.intent,
            )?;
            let outcome = verified_repository_publication_outcome(publication, &command)?;
            return complete_cleanup(
                &mut publication.cleanup,
                session_id,
                receipts,
                Some(outcome),
            );
        }
        complete_cleanup(self, session_id, receipts, None)
    }

    /// Normalize only a complete canonical receipt set already present in a
    /// historical Requested row. No caller-provided Runtime report crosses
    /// this compatibility boundary.
    pub(crate) fn normalize_legacy_completion(
        &mut self,
        session_id: &str,
    ) -> Result<bool, SessionCleanupError> {
        let receipts = self.recorded_receipts(session_id)?;
        self.complete(session_id, &receipts)
    }
}
