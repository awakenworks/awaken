//! Domain-owned operation vocabulary for terminal Session cleanup.
//!
//! The Session aggregate owns whether cleanup is required or complete. Runtime
//! implementations own the substrate-specific effects, but must execute them
//! from a stable [`SessionCleanupCommand`]. Their untrusted completion report
//! becomes a [`VerifiedSessionCleanupReceipt`] only after exact command binding.

mod completion;
mod repository_publication;

pub use completion::{SessionCleanupCompletion, VerifiedSessionCleanupReceipt};

use awaken_provisioning_contract::{
    RepositoryPublicationExpectation, RepositoryPublicationReceipt,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One explicit, frozen request to publish the exact writable Repository input
/// already owned by the Session. Configuration, credential reference, mount,
/// and access remain in the existing [`crate::ResolvedInput`] authority; this
/// intent adds only the terminal Git ref expected by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRepositoryPublicationIntent {
    pub input: crate::ResolvedInput,
    pub expectation: RepositoryPublicationExpectation,
}

/// Stable root-only publication command derived from the one durable cleanup
/// operation. It contains no credential material and cannot select a current
/// Repository configuration after the Session has frozen its input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRepositoryPublicationCommand {
    pub session_id: String,
    pub effect_id: String,
    pub intent: SessionRepositoryPublicationIntent,
}

/// Untrusted but canonical, secret-free effect evidence for one exact Session
/// publication command. The Repository result remains owned by the provisioning
/// contract; this wrapper binds it to the durable Session operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRepositoryPublicationReceipt {
    pub command_fingerprint: String,
    pub effect_receipt: RepositoryPublicationReceipt,
    pub receipt_fingerprint: String,
}

/// The publication sidecar around the one legacy cleanup operation.
///
/// Its fields are private so callers cannot construct a recursive wrapper or a
/// second cleanup state machine. The inner operation remains the sole phase,
/// target, completion, and terminal-receipt authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRepositoryPublicationCleanup {
    cleanup: SessionCleanupOperation,
    intent: SessionRepositoryPublicationIntent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    receipt: Option<SessionRepositoryPublicationReceipt>,
}

/// Heap-free admission kernel for one terminal cleanup effect receipt. The
/// typed boundary performs the exact identity and canonical-fingerprint
/// comparisons; this closed rule makes every required axis explicit and is
/// shared by production verification and exhaustive checking.
#[must_use]
pub(crate) const fn session_cleanup_completion_admitted(
    artifact_effects_unique: bool,
    session_matches: bool,
    thread_matches: bool,
    effect_matches: bool,
    canonical_receipt_matches: bool,
) -> bool {
    artifact_effects_unique
        && session_matches
        && thread_matches
        && effect_matches
        && canonical_receipt_matches
}

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

/// The one durable cleanup operation stored by the Session aggregate.
///
/// The four pre-publication variants and their field types are retained exactly
/// for Rust source and persisted-wire compatibility. Repository publication is
/// an additive heap-indirected sidecar around one of those same variants; it
/// delegates every cleanup phase transition to that sole inner operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionCleanupOperation {
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
        /// Canonical Runtime completions already admitted for this frozen
        /// target set. Local execution may settle the whole operation in one
        /// call; a remote Worker records these one at a time through the same
        /// operation so process loss never requires a second cleanup queue.
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
    /// Additive publication metadata around exactly one non-publication cleanup
    /// operation. The private payload and custom decoder reject recursive
    /// wrappers, so this cannot become a parallel phase hierarchy.
    RepositoryPublication(Box<SessionRepositoryPublicationCleanup>),
}

impl SessionCleanupOperation {
    #[must_use]
    pub(crate) fn phase(&self) -> SessionCleanupPhase {
        match self.legacy_cleanup() {
            Self::NotRequested => SessionCleanupPhase::NotRequested,
            Self::Fenced { .. } => SessionCleanupPhase::Fenced,
            Self::Requested { .. } => SessionCleanupPhase::Requested,
            Self::Completed { .. } => SessionCleanupPhase::Completed,
            Self::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
        }
    }

    fn legacy_cleanup(&self) -> &Self {
        match self {
            Self::RepositoryPublication(publication) => &publication.cleanup,
            _ => self,
        }
    }

    fn effect_id(&self) -> Option<&str> {
        match self.legacy_cleanup() {
            Self::Fenced { effect_id }
            | Self::Requested { effect_id, .. }
            | Self::Completed { effect_id, .. } => Some(effect_id),
            Self::NotRequested => None,
            Self::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
        }
    }

    fn advance_to(&mut self, next: Self) -> bool {
        if matches!(self, Self::RepositoryPublication(_))
            || matches!(next, Self::RepositoryPublication(_))
        {
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
        self.advance_to(Self::Fenced {
            effect_id: cleanup_effect_id(session_id),
        })
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
        match self {
            Self::NotRequested => {
                let mut cleanup = Self::default();
                if !cleanup.request(session_id) {
                    return Err(SessionCleanupError::InvalidPhaseAdvance);
                }
                *self =
                    Self::RepositoryPublication(Box::new(SessionRepositoryPublicationCleanup {
                        cleanup,
                        intent: repository_publication,
                        receipt: None,
                    }));
                Ok(true)
            }
            Self::RepositoryPublication(publication) => {
                if publication.cleanup.effect_id() == Some(cleanup_effect_id(session_id).as_str())
                    && publication.intent == repository_publication
                {
                    Ok(false)
                } else {
                    Err(SessionCleanupError::FrozenRepositoryPublicationMismatch)
                }
            }
            Self::Fenced { .. } | Self::Requested { .. } | Self::Completed { .. } => {
                Err(SessionCleanupError::FrozenRepositoryPublicationMismatch)
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
        if let Self::RepositoryPublication(publication) = self {
            return publication.cleanup.freeze_targets(
                session_id,
                thread_ids,
                delegation_watermark,
                runtime_commit_cursor,
            );
        }
        let Self::Fenced { effect_id } = self else {
            return match self {
                Self::Requested {
                    thread_ids: durable,
                    delegation_watermark: durable_watermark,
                    runtime_commit_cursor: durable_cursor,
                    ..
                }
                | Self::Completed {
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
                Self::NotRequested => Err(SessionCleanupError::NotRequested),
                Self::Fenced { .. } => unreachable!(),
                Self::RepositoryPublication(_) => unreachable!(),
            };
        };
        if *effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        let effect_id = effect_id.clone();
        let mut durable = BTreeSet::from([session_id.to_string()]);
        durable.extend(thread_ids);
        let advanced = self.advance_to(Self::Requested {
            effect_id,
            thread_ids: durable,
            delegation_watermark,
            runtime_commit_cursor: Some(runtime_commit_cursor),
            completions: BTreeMap::new(),
        });
        if !advanced {
            return Err(SessionCleanupError::InvalidPhaseAdvance);
        }
        Ok(advanced)
    }

    #[must_use]
    pub fn thread_ids(&self) -> Option<&BTreeSet<String>> {
        match self.legacy_cleanup() {
            Self::Requested { thread_ids, .. } | Self::Completed { thread_ids, .. } => {
                Some(thread_ids)
            }
            Self::NotRequested | Self::Fenced { .. } => None,
            Self::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
        }
    }

    /// Immutable terminal projection boundary after Runtime quiescence.
    #[must_use]
    pub fn runtime_commit_cursor(&self) -> Option<u64> {
        match self.legacy_cleanup() {
            Self::Requested {
                runtime_commit_cursor,
                ..
            }
            | Self::Completed {
                runtime_commit_cursor,
                ..
            } => *runtime_commit_cursor,
            Self::NotRequested | Self::Fenced { .. } => None,
            Self::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
        }
    }

    #[must_use]
    pub fn repository_publication_intent(&self) -> Option<&SessionRepositoryPublicationIntent> {
        match self {
            Self::RepositoryPublication(publication) => Some(&publication.intent),
            Self::NotRequested
            | Self::Fenced { .. }
            | Self::Requested { .. }
            | Self::Completed { .. } => None,
        }
    }

    #[must_use]
    pub fn repository_publication_receipt(&self) -> Option<&SessionRepositoryPublicationReceipt> {
        match self {
            Self::RepositoryPublication(publication) => publication.receipt.as_ref(),
            Self::NotRequested
            | Self::Fenced { .. }
            | Self::Requested { .. }
            | Self::Completed { .. } => None,
        }
    }

    #[must_use]
    pub fn is_fenced(&self) -> bool {
        matches!(self.legacy_cleanup(), Self::Fenced { .. })
    }

    #[must_use]
    pub fn is_requested(&self) -> bool {
        matches!(self.legacy_cleanup(), Self::Requested { .. })
    }

    #[must_use]
    pub fn is_completed(&self) -> bool {
        matches!(self.legacy_cleanup(), Self::Completed { .. })
    }

    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        matches!(
            self.legacy_cleanup(),
            Self::Fenced { .. } | Self::Requested { .. }
        )
    }

    /// Project the one root Repository publication effect only after every
    /// child Runtime cleanup receipt is durable and before the root cleanup is
    /// allowed to dispose the shared environment.
    pub fn publication_command(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionRepositoryPublicationCommand>, SessionCleanupError> {
        let Self::RepositoryPublication(publication) = self else {
            return match self {
                Self::Requested { .. } | Self::Completed { .. } => Ok(None),
                Self::NotRequested | Self::Fenced { .. } => Err(SessionCleanupError::NotRequested),
                Self::RepositoryPublication(_) => unreachable!(),
            };
        };
        let Self::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = &publication.cleanup
        else {
            return if publication.cleanup.is_completed() {
                Ok(None)
            } else {
                Err(SessionCleanupError::NotRequested)
            };
        };
        if *effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        if !thread_ids.contains(session_id) {
            return Err(SessionCleanupError::FrozenTargetsMismatch);
        }
        if thread_ids
            .iter()
            .any(|thread_id| thread_id != session_id && !completions.contains_key(thread_id))
        {
            return Ok(None);
        }
        let command =
            SessionRepositoryPublicationCommand::new(session_id, effect_id, &publication.intent)?;
        if let Some(receipt) = publication.receipt.as_ref() {
            receipt.verify(&command)?;
            return Ok(None);
        }
        Ok(Some(command))
    }

    #[must_use]
    pub fn command_for(&self, session_id: &str, thread_id: &str) -> Option<SessionCleanupCommand> {
        let cleanup = self.legacy_cleanup();
        let Self::Requested {
            effect_id,
            thread_ids,
            ..
        } = cleanup
        else {
            return None;
        };
        if !thread_ids.contains(thread_id) {
            return None;
        }
        Some(SessionCleanupCommand::new(session_id, thread_id, effect_id))
    }

    /// Commands in the immutable target set that have no verified completion
    /// yet. This is the sole durable remote-work projection: callers may poll it,
    /// but cannot add targets or author another cleanup registry.
    pub fn pending_commands(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionCleanupCommand>, SessionCleanupError> {
        let cleanup = self.legacy_cleanup();
        let Self::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = cleanup
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
            // finalizer until the exact publication receipt is durable.
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

    /// Verify and durably retain the exact root Repository publication receipt.
    /// Children must already be complete. Exact replay is a no-op, while a
    /// different command binding or effect receipt fails closed.
    pub fn record_repository_publication_receipt(
        &mut self,
        session_id: &str,
        receipt: SessionRepositoryPublicationReceipt,
    ) -> Result<bool, SessionCleanupError> {
        let Self::RepositoryPublication(publication) = self else {
            return match self {
                Self::Fenced { .. } => Err(SessionCleanupError::RepositoryPublicationNotReady),
                Self::NotRequested | Self::Requested { .. } | Self::Completed { .. } => {
                    Err(SessionCleanupError::RepositoryPublicationNotRequested)
                }
                Self::RepositoryPublication(_) => unreachable!(),
            };
        };
        let publication = publication.as_mut();
        let (effect_id, root_present, children_complete, completed) = match &publication.cleanup {
            Self::Requested {
                effect_id,
                thread_ids,
                completions,
                ..
            } => (
                effect_id.clone(),
                thread_ids.contains(session_id),
                thread_ids.iter().all(|thread_id| {
                    thread_id == session_id || completions.contains_key(thread_id)
                }),
                false,
            ),
            Self::Completed { effect_id, .. } => (effect_id.clone(), true, true, true),
            Self::NotRequested => {
                return Err(SessionCleanupError::RepositoryPublicationNotRequested);
            }
            Self::Fenced { .. } => {
                return Err(SessionCleanupError::RepositoryPublicationNotReady);
            }
            Self::RepositoryPublication(_) => {
                return Err(SessionCleanupError::FrozenRepositoryPublicationMismatch);
            }
        };
        if effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        if !root_present {
            return Err(SessionCleanupError::FrozenTargetsMismatch);
        }
        if !children_complete {
            return Err(SessionCleanupError::RepositoryPublicationNotReady);
        }
        let command =
            SessionRepositoryPublicationCommand::new(session_id, &effect_id, &publication.intent)?;
        receipt.verify(&command)?;
        match publication.receipt.as_ref() {
            Some(durable) if durable == &receipt => return Ok(false),
            Some(_) => {
                return Err(SessionCleanupError::RepositoryPublicationReceiptMismatch);
            }
            None if completed => {
                return Err(SessionCleanupError::MissingRepositoryPublicationReceipt);
            }
            None => {}
        }
        publication.receipt = Some(receipt);
        Ok(true)
    }

    /// Verify and durably retain one completion for an already-frozen target.
    /// Exact replay is a no-op; a conflicting completion fails closed.
    pub fn record_completion(
        &mut self,
        session_id: &str,
        completion: SessionCleanupCompletion,
    ) -> Result<bool, SessionCleanupError> {
        if let Self::RepositoryPublication(publication) = self {
            if completion.thread_id == session_id {
                let effect_id = publication
                    .cleanup
                    .effect_id()
                    .ok_or(SessionCleanupError::NotRequested)?;
                let receipt = publication
                    .receipt
                    .as_ref()
                    .ok_or(SessionCleanupError::MissingRepositoryPublicationReceipt)?;
                let command = SessionRepositoryPublicationCommand::new(
                    session_id,
                    effect_id,
                    &publication.intent,
                )?;
                receipt.verify(&command)?;
            }
            return publication
                .cleanup
                .record_completion(session_id, completion);
        }
        let (effect_id, thread_ids) = match self {
            Self::Requested {
                effect_id,
                thread_ids,
                ..
            }
            | Self::Completed {
                effect_id,
                thread_ids,
                ..
            } => (effect_id, thread_ids),
            Self::NotRequested | Self::Fenced { .. } => {
                return Err(SessionCleanupError::NotRequested);
            }
            Self::RepositoryPublication(_) => unreachable!(),
        };
        if !thread_ids.contains(&completion.thread_id) {
            return Err(SessionCleanupError::ReceiptMismatch);
        }
        let command = SessionCleanupCommand::new(session_id, &completion.thread_id, effect_id);
        completion.verify(&command)?;
        if self.is_completed() {
            return Ok(false);
        }
        let Self::Requested { completions, .. } = self else {
            return Err(SessionCleanupError::NotRequested);
        };
        match completions.get(&completion.thread_id) {
            Some(durable) if durable == &completion => Ok(false),
            Some(_) => Err(SessionCleanupError::ReceiptMismatch),
            None => {
                completions.insert(completion.thread_id.clone(), completion);
                Ok(true)
            }
        }
    }

    /// Re-verify every durable remote completion against the immutable command
    /// set before it can become aggregate completion evidence.
    pub fn recorded_receipts(
        &self,
        session_id: &str,
    ) -> Result<Vec<VerifiedSessionCleanupReceipt>, SessionCleanupError> {
        let cleanup = self.legacy_cleanup();
        let Self::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = cleanup
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

    /// Commit verified receipt evidence only after every thread effect has
    /// succeeded. Raw Runtime completions cannot cross this boundary.
    pub fn complete(
        &mut self,
        session_id: &str,
        receipts: &[VerifiedSessionCleanupReceipt],
    ) -> Result<bool, SessionCleanupError> {
        if let Self::RepositoryPublication(publication) = self {
            let publication = publication.as_mut();
            let effect_id = publication
                .cleanup
                .effect_id()
                .ok_or(SessionCleanupError::NotRequested)?;
            let receipt = publication
                .receipt
                .as_ref()
                .ok_or(SessionCleanupError::MissingRepositoryPublicationReceipt)?;
            let command = SessionRepositoryPublicationCommand::new(
                session_id,
                effect_id,
                &publication.intent,
            )?;
            receipt.verify(&command)?;
            return complete_cleanup(
                &mut publication.cleanup,
                session_id,
                receipts,
                Some(receipt),
            );
        }
        complete_cleanup(self, session_id, receipts, None)
    }
}

fn complete_cleanup(
    cleanup: &mut SessionCleanupOperation,
    session_id: &str,
    receipts: &[VerifiedSessionCleanupReceipt],
    repository_publication_receipt: Option<&SessionRepositoryPublicationReceipt>,
) -> Result<bool, SessionCleanupError> {
    let SessionCleanupOperation::Requested {
        effect_id,
        thread_ids,
        delegation_watermark,
        runtime_commit_cursor,
        ..
    } = cleanup
    else {
        return if cleanup.is_completed() {
            Ok(false)
        } else {
            Err(SessionCleanupError::NotRequested)
        };
    };
    if receipts.is_empty() {
        return Err(SessionCleanupError::MissingReceipt);
    }
    let mut evidence = receipts.to_vec();
    evidence.sort_by(|left, right| left.thread_id().cmp(right.thread_id()));
    for pair in evidence.windows(2) {
        if pair[0].thread_id() == pair[1].thread_id() {
            return Err(SessionCleanupError::DuplicateThread(
                pair[0].thread_id().to_string(),
            ));
        }
    }
    let evidenced_threads = evidence
        .iter()
        .map(|receipt| receipt.thread_id().to_string())
        .collect::<BTreeSet<_>>();
    if !evidenced_threads.contains(session_id) {
        return Err(SessionCleanupError::MissingRootReceipt);
    }
    if evidenced_threads != *thread_ids {
        return Err(SessionCleanupError::MissingReceipt);
    }
    for receipt in &evidence {
        let expected = SessionCleanupCommand::new(session_id, receipt.thread_id(), effect_id);
        if receipt.command() != &expected {
            return Err(SessionCleanupError::ReceiptMismatch);
        }
    }
    let cleanup_evidence = evidence
        .iter()
        .map(|receipt| {
            (
                receipt.thread_id(),
                receipt.effect_id(),
                receipt.receipt_fingerprint(),
            )
        })
        .collect::<Vec<_>>();
    let receipt_fingerprint = if let Some(publication_receipt) = repository_publication_receipt {
        crate::stable_fingerprint(&(
            "session-terminal-cleanup-receipt-v2",
            effect_id.as_str(),
            *delegation_watermark,
            cleanup_evidence,
            publication_receipt.receipt_fingerprint.as_str(),
        ))
    } else {
        crate::stable_fingerprint(&(
            "session-terminal-cleanup-receipt-v1",
            effect_id.as_str(),
            *delegation_watermark,
            cleanup_evidence,
        ))
    };
    let next = SessionCleanupOperation::Completed {
        effect_id: effect_id.clone(),
        thread_ids: thread_ids.clone(),
        delegation_watermark: *delegation_watermark,
        runtime_commit_cursor: *runtime_commit_cursor,
        receipt_fingerprint,
    };
    if !cleanup.advance_to(next) {
        return Err(SessionCleanupError::InvalidPhaseAdvance);
    }
    Ok(true)
}

/// Stable, per-thread cleanup command derived from the root operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCleanupCommand {
    pub session_id: String,
    pub thread_id: String,
    pub effect_id: String,
}

impl SessionCleanupCommand {
    #[must_use]
    pub fn new(session_id: &str, thread_id: &str, root_effect_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            thread_id: thread_id.to_string(),
            effect_id: crate::stable_fingerprint(&(
                "session-terminal-cleanup-thread-v1",
                session_id,
                thread_id,
                root_effect_id,
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionCleanupError {
    #[error("Session terminal cleanup was not requested")]
    NotRequested,
    #[error("Session terminal cleanup has no Runtime receipt")]
    MissingReceipt,
    #[error("Session terminal cleanup has no root-thread receipt")]
    MissingRootReceipt,
    #[error("Session terminal cleanup contains duplicate thread {0}")]
    DuplicateThread(String),
    #[error("Session terminal cleanup receipt does not match its exact intent")]
    ReceiptMismatch,
    #[error("invalid Session Repository publication intent: {0}")]
    InvalidRepositoryPublicationIntent(String),
    #[error("Session Repository publication was not requested")]
    RepositoryPublicationNotRequested,
    #[error("Session Repository publication is not ready before child cleanup completes")]
    RepositoryPublicationNotReady,
    #[error("Session terminal cleanup has no Repository publication receipt")]
    MissingRepositoryPublicationReceipt,
    #[error("Session Repository publication receipt does not match its exact command")]
    RepositoryPublicationReceiptMismatch,
    #[error("Session Repository publication intent is already frozen to another value")]
    FrozenRepositoryPublicationMismatch,
    #[error("Session cleanup operation does not match its Session identity")]
    OperationMismatch,
    #[error("Session terminal cleanup targets were already frozen at a different watermark")]
    FrozenTargetsMismatch,
    #[error("Session cleanup operation attempted an invalid phase advance")]
    InvalidPhaseAdvance,
}

fn cleanup_effect_id(session_id: &str) -> String {
    crate::stable_fingerprint(&("session-terminal-cleanup-v1", session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::{
        RepositoryPublicationExpectation, RepositoryPublicationReceipt,
    };
    use awaken_resource_contract::ResourceAccess;
    use proptest::prelude::*;

    fn verified(command: &SessionCleanupCommand) -> VerifiedSessionCleanupReceipt {
        SessionCleanupCompletion::new(command, Vec::new())
            .verify(command)
            .unwrap()
    }

    fn publication_intent() -> SessionRepositoryPublicationIntent {
        SessionRepositoryPublicationIntent {
            input: crate::ResolvedInput {
                binding_id: awaken_resource_contract::BindingId::from("source"),
                source: crate::ResolvedInputSource::Repository {
                    repository_id: awaken_resource_contract::RepositoryId::from("repo-1"),
                    config: awaken_resource_contract::RepositoryConfigVersion {
                        repository_id: awaken_resource_contract::RepositoryId::from("repo-1"),
                        version: awaken_resource_contract::ConfigVersion(7),
                        remote_url: "https://example.test/repo.git".into(),
                        credential_binding: None,
                        initial_branch: Some("main".into()),
                        initial_commit: None,
                        clone_policy: Default::default(),
                    },
                    credential: None,
                },
                mount_path: "/workspace/source".into(),
                access: ResourceAccess::ReadWrite,
                instructions: None,
            },
            expectation: RepositoryPublicationExpectation {
                branch: "awf/work".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
            },
        }
    }

    fn publication_receipt(
        command: &SessionRepositoryPublicationCommand,
    ) -> SessionRepositoryPublicationReceipt {
        let (repository_id, remote_url) = command.intent.repository_target().unwrap();
        SessionRepositoryPublicationReceipt::new(
            command,
            RepositoryPublicationReceipt {
                repository_id: repository_id.to_string(),
                source_remote_url: remote_url.to_string(),
                branch: command.intent.expectation.branch.clone(),
                commit: command.intent.expectation.commit.clone(),
            },
        )
    }

    #[allow(dead_code)]
    enum LegacySessionCleanupOperationLayout {
        NotRequested,
        Fenced {
            effect_id: String,
        },
        Requested {
            effect_id: String,
            thread_ids: BTreeSet<String>,
            delegation_watermark: u64,
            runtime_commit_cursor: Option<u64>,
            completions: BTreeMap<String, SessionCleanupCompletion>,
        },
        Completed {
            effect_id: String,
            thread_ids: BTreeSet<String>,
            delegation_watermark: u64,
            runtime_commit_cursor: Option<u64>,
            receipt_fingerprint: String,
        },
    }

    #[test]
    fn cleanup_layout_and_legacy_variant_fields_remain_exact() {
        // Layout cause/effect decision table: C1 PersistedSession embeds the
        // cleanup operation by value; C2 the four legacy variants retain their
        // exact public fields; C3 publication is absent/present. Effects: E1 the
        // operation and PersistedSession retain their exact legacy inline sizes;
        // E2 legacy Rust construction/destructuring keeps BTreeMap/String field
        // types; E3 publication contributes only one boxed-wrapper word; E4
        // serde emits the wrapper payload without a Box representation.
        //
        // | Rule | publication | legacy fields | Effect |
        // | B1 | absent | unchanged | E1/E2 |
        // | B2 | present | isolated in wrapper | E1/E3/E4 |
        //
        // On x86_64 the uncorrected publication layout measured 120 bytes for
        // SessionCleanupOperation and 2480 for PersistedSession. Boxing the one
        // additive wrapper, not either legacy public field, preserves the exact
        // 104/2464-byte layout.
        assert_eq!(
            std::mem::size_of::<SessionCleanupOperation>(),
            std::mem::size_of::<LegacySessionCleanupOperationLayout>(),
            "B1-B2/E1 publication must not enlarge the legacy cleanup layout"
        );
        assert!(
            std::mem::size_of::<SessionCleanupOperation>()
                < std::mem::size_of::<SessionRepositoryPublicationReceipt>(),
            "B1-B2/E3 cleanup must not inline publication payloads"
        );
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<SessionCleanupOperation>(), 104, "E1");
            assert_eq!(std::mem::size_of::<crate::PersistedSession>(), 2464, "E1");
        }
        let _legacy_requested_source_shape = SessionCleanupOperation::Requested {
            effect_id: String::new(),
            thread_ids: BTreeSet::new(),
            delegation_watermark: 0,
            runtime_commit_cursor: None,
            completions: BTreeMap::new(),
        };
        let _legacy_completed_source_shape = SessionCleanupOperation::Completed {
            effect_id: String::new(),
            thread_ids: BTreeSet::new(),
            delegation_watermark: 0,
            runtime_commit_cursor: None,
            receipt_fingerprint: String::new(),
        };

        let intent = publication_intent();
        let mut state = SessionCleanupOperation::default();
        state
            .request_with_publication("boxed-wire", intent.clone())
            .unwrap();
        let encoded = serde_json::to_value(&state).unwrap();
        assert_eq!(
            encoded.get("intent"),
            Some(&serde_json::to_value(intent).unwrap()),
            "B2/E4"
        );
    }

    #[test]
    fn publication_wrapper_deserialization_rejects_recursive_or_impossible_state() {
        // Decoder cause/effect decision table: C1 the additive wrapper contains
        // exactly one legacy cleanup; C2 that inner cleanup is itself a wrapper;
        // C3 the inner cleanup has not been requested; C4 a completed inner
        // cleanup has no publication receipt. E1 admits the one-level shape;
        // E2 rejects before a recursive or phase-inconsistent authority enters
        // PersistedSession.
        //
        // | Rule | inner cleanup | receipt shape | Effect |
        // | D1 | Fenced legacy | absent | E1 |
        // | D2 | publication wrapper | absent | E2 nested |
        // | D3 | NotRequested | absent | E2 unrequested |
        // | D4 | Completed legacy | absent | E2 missing receipt |
        let intent = publication_intent();
        let mut state = SessionCleanupOperation::default();
        state
            .request_with_publication("decode-shape", intent.clone())
            .unwrap();
        let valid = serde_json::to_value(&state).unwrap();
        assert!(
            serde_json::from_value::<SessionCleanupOperation>(valid.clone()).is_ok(),
            "D1/E1"
        );

        let nested = serde_json::json!({
            "state": "repository_publication",
            "cleanup": valid,
            "intent": intent,
        });
        let nested_error =
            serde_json::from_value::<SessionCleanupOperation>(nested).expect_err("D2/E2");
        assert!(
            nested_error
                .to_string()
                .contains("nested Repository publication cleanup is forbidden"),
            "D2/E2: {nested_error}"
        );

        let unrequested = serde_json::json!({
            "state": "repository_publication",
            "cleanup": { "state": "not_requested" },
            "intent": publication_intent(),
        });
        assert!(
            serde_json::from_value::<SessionCleanupOperation>(unrequested).is_err(),
            "D3/E2"
        );

        let completed_without_receipt = serde_json::json!({
            "state": "repository_publication",
            "cleanup": {
                "state": "completed",
                "effect_id": cleanup_effect_id("decode-shape"),
                "thread_ids": ["decode-shape"],
                "delegation_watermark": 0,
                "runtime_commit_cursor": 0,
                "receipt_fingerprint": "fnv1a64:0000000000000000"
            },
            "intent": publication_intent(),
        });
        assert!(
            serde_json::from_value::<SessionCleanupOperation>(completed_without_receipt).is_err(),
            "D4/E2"
        );
    }

    #[test]
    fn intent_and_receipt_are_stable_across_recovery() {
        let mut state = SessionCleanupOperation::default();
        assert!(state.request("session-1"));
        assert!(state.is_fenced());
        assert!(state.freeze_targets("session-1", [], 7, 13).unwrap());
        let first = state.command_for("session-1", "session-1").unwrap();
        let replay = state.command_for("session-1", "session-1").unwrap();
        assert_eq!(first, replay);
        let receipt = verified(&first);
        assert!(state.complete("session-1", &[receipt]).unwrap());
        assert!(state.is_completed());
        assert_eq!(
            state.runtime_commit_cursor(),
            Some(13),
            "the quiescent Runtime high-water survives Requested -> Completed"
        );
    }

    #[test]
    fn legacy_no_publication_wire_and_fingerprints_remain_exact_v1() {
        // Compatibility cause/effect decision table: C1 an existing caller uses
        // `request`; C2 no Repository publication fields exist. Effects: E1 the
        // Fenced/Requested/Completed JSON bytes remain exact; E2 the root effect,
        // thread command, thread completion, and v1 aggregate receipt identities
        // retain their pre-publication values; E3 legacy JSON decodes without
        // inventing an intent.
        //
        // | Rule | request API | publication fields | phase | Effect |
        // | L1 | legacy | absent | Fenced | E1/E2 |
        // | L2 | legacy | absent | Requested | E1/E2/E3 |
        // | L3 | legacy | absent | Completed | E1/E2/E3 |
        //
        // These constants were produced by the authoritative v1 implementation
        // before optional Repository publication fields were introduced.
        let mut state = SessionCleanupOperation::default();
        assert!(state.request("legacy-session"));
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            r#"{"state":"fenced","effect_id":"fnv1a64:0f89047907a93a47"}"#,
            "L1/E1"
        );

        assert!(state.freeze_targets("legacy-session", [], 7, 11).unwrap());
        let requested = r#"{"state":"requested","effect_id":"fnv1a64:0f89047907a93a47","thread_ids":["legacy-session"],"delegation_watermark":7,"runtime_commit_cursor":11}"#;
        assert_eq!(serde_json::to_string(&state).unwrap(), requested, "L2/E1");
        let decoded: SessionCleanupOperation = serde_json::from_str(requested).unwrap();
        assert!(decoded.repository_publication_intent().is_none(), "L2/E3");

        let command = state
            .command_for("legacy-session", "legacy-session")
            .unwrap();
        assert_eq!(command.effect_id, "fnv1a64:34c458e86e3a9559", "L2/E2");
        let completion = SessionCleanupCompletion::new(&command, Vec::new());
        assert_eq!(
            completion.receipt_fingerprint, "fnv1a64:d5eb131b9e72fe50",
            "L2/E2"
        );
        assert!(
            state
                .complete("legacy-session", &[completion.verify(&command).unwrap()])
                .unwrap()
        );
        let completed = r#"{"state":"completed","effect_id":"fnv1a64:0f89047907a93a47","thread_ids":["legacy-session"],"delegation_watermark":7,"runtime_commit_cursor":11,"receipt_fingerprint":"fnv1a64:b5e378ea35d2300b"}"#;
        assert_eq!(serde_json::to_string(&state).unwrap(), completed, "L3/E1");
        let decoded: SessionCleanupOperation = serde_json::from_str(completed).unwrap();
        assert!(decoded.repository_publication_intent().is_none(), "L3/E3");
        assert!(decoded.repository_publication_receipt().is_none(), "L3/E3");
    }

    #[test]
    fn repository_publication_is_child_first_durable_and_root_gated() {
        // Cause/effect graph: C1 an explicit valid publication intent is frozen;
        // C2 child cleanup is pending/complete; C3 publication receipt is
        // absent/present; C4 root cleanup is asserted; C5 process recovery may
        // occur after either durable effect. Effects: E1 children are the only
        // first commands; E2 publication becomes the sole root projection; E3
        // ordinary root finalization is withheld until publication evidence; E4
        // exact receipts replay without a second effect; E5 recovery preserves
        // the same command/receipt and reaches one v2 terminal outcome.
        //
        // | Rule | child | publication receipt | root cleanup | restart | Effect |
        // | R1 | pending | absent | no | no | E1 |
        // | R2 | complete | absent | no | yes | E2/E3/E5 |
        // | R3 | complete | absent | asserted | no | reject E3 |
        // | R4 | complete | exact new/replay | no | yes | E4/E5 |
        // | R5 | complete | present | asserted | no | admit root |
        // | R6 | complete | present | complete | yes | one v2 terminal fact |
        let mut state = SessionCleanupOperation::default();
        let intent = publication_intent();
        assert!(
            state
                .request_with_publication("publish-session", intent.clone())
                .unwrap()
        );
        assert!(
            !state
                .request_with_publication("publish-session", intent)
                .unwrap(),
            "R1 exact request replay"
        );
        state
            .freeze_targets("publish-session", ["publish-child".to_string()], 17, 23)
            .unwrap();
        let child = state
            .command_for("publish-session", "publish-child")
            .unwrap();
        let root = state
            .command_for("publish-session", "publish-session")
            .unwrap();
        assert_eq!(
            state
                .pending_commands("publish-session")
                .unwrap()
                .iter()
                .map(|command| command.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["publish-child"],
            "R1/E1"
        );
        assert!(
            state
                .publication_command("publish-session")
                .unwrap()
                .is_none(),
            "R1/E1"
        );
        let early_command = SessionRepositoryPublicationCommand::new(
            "publish-session",
            &cleanup_effect_id("publish-session"),
            state.repository_publication_intent().unwrap(),
        )
        .unwrap();
        assert_eq!(
            state.record_repository_publication_receipt(
                "publish-session",
                publication_receipt(&early_command),
            ),
            Err(SessionCleanupError::RepositoryPublicationNotReady),
            "R1 children cannot be bypassed"
        );

        state
            .record_completion(
                "publish-session",
                SessionCleanupCompletion::new(&child, Vec::new()),
            )
            .unwrap();
        assert!(
            state
                .pending_commands("publish-session")
                .unwrap()
                .is_empty(),
            "R2/E3"
        );
        let publication = state
            .publication_command("publish-session")
            .unwrap()
            .expect("R2/E2");
        assert_eq!(publication.session_id, "publish-session", "R2 root-only");
        assert_eq!(
            state.record_completion(
                "publish-session",
                SessionCleanupCompletion::new(&root, Vec::new()),
            ),
            Err(SessionCleanupError::MissingRepositoryPublicationReceipt),
            "R3/E3"
        );

        let encoded = serde_json::to_vec(&state).unwrap();
        let mut recovered: SessionCleanupOperation = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            recovered.publication_command("publish-session").unwrap(),
            Some(publication.clone()),
            "R2/E5"
        );
        let receipt = publication_receipt(&publication);
        assert!(
            recovered
                .record_repository_publication_receipt("publish-session", receipt.clone())
                .unwrap(),
            "R4/E4"
        );
        assert!(
            !recovered
                .record_repository_publication_receipt("publish-session", receipt.clone())
                .unwrap(),
            "R4/E4 exact replay"
        );

        let encoded = serde_json::to_vec(&recovered).unwrap();
        let mut recovered: SessionCleanupOperation = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            recovered
                .pending_commands("publish-session")
                .unwrap()
                .iter()
                .map(|command| command.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["publish-session"],
            "R5/E5"
        );
        assert!(
            recovered
                .publication_command("publish-session")
                .unwrap()
                .is_none(),
            "R5"
        );
        recovered
            .record_completion(
                "publish-session",
                SessionCleanupCompletion::new(&root, Vec::new()),
            )
            .unwrap();
        let receipts = recovered.recorded_receipts("publish-session").unwrap();
        assert!(
            recovered.complete("publish-session", &receipts).unwrap(),
            "R6"
        );
        assert!(recovered.repository_publication_receipt().is_some(), "R6");
        let SessionCleanupOperation::RepositoryPublication(publication) = &recovered else {
            panic!("R6 publication wrapper");
        };
        let SessionCleanupOperation::Completed {
            receipt_fingerprint,
            ..
        } = &publication.cleanup
        else {
            panic!("R6 completed");
        };
        let cleanup_evidence = receipts
            .iter()
            .map(|cleanup| {
                (
                    cleanup.thread_id(),
                    cleanup.effect_id(),
                    cleanup.receipt_fingerprint(),
                )
            })
            .collect::<Vec<_>>();
        let would_be_v1 = crate::stable_fingerprint(&(
            "session-terminal-cleanup-receipt-v1",
            cleanup_effect_id("publish-session"),
            17_u64,
            cleanup_evidence.clone(),
        ));
        let expected_v2 = crate::stable_fingerprint(&(
            "session-terminal-cleanup-receipt-v2",
            cleanup_effect_id("publish-session"),
            17_u64,
            cleanup_evidence,
            receipt.receipt_fingerprint.as_str(),
        ));
        assert_eq!(receipt_fingerprint.as_str(), expected_v2, "R6 v2");
        assert_eq!(
            receipt_fingerprint, "fnv1a64:98d50a5f30ce7e68",
            "R6 exact v2"
        );
        assert_ne!(receipt_fingerprint.as_str(), would_be_v1, "R6 not v1");
        let encoded = serde_json::to_vec(&recovered).unwrap();
        let mut recovered: SessionCleanupOperation = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            serde_json::to_string(&recovered).unwrap(),
            r#"{"state":"repository_publication","cleanup":{"state":"completed","effect_id":"fnv1a64:0644e194c63acc8b","thread_ids":["publish-child","publish-session"],"delegation_watermark":17,"runtime_commit_cursor":23,"receipt_fingerprint":"fnv1a64:98d50a5f30ce7e68"},"intent":{"input":{"binding_id":"source","source":{"kind":"repository","repository_id":"repo-1","config":{"repository_id":"repo-1","version":7,"remote_url":"https://example.test/repo.git","initial_branch":"main","clone_policy":{}}},"mount_path":"/workspace/source","access":"read_write"},"expectation":{"branch":"awf/work","commit":"0123456789abcdef0123456789abcdef01234567"}},"receipt":{"command_fingerprint":"fnv1a64:e390852b7b991ab6","effect_receipt":{"repository_id":"repo-1","source_remote_url":"https://example.test/repo.git","branch":"awf/work","commit":"0123456789abcdef0123456789abcdef01234567"},"receipt_fingerprint":"fnv1a64:2350653aaf1ff920"}}"#,
            "R6 exact v2 JSON"
        );
        assert!(
            !recovered
                .record_repository_publication_receipt("publish-session", receipt)
                .unwrap(),
            "R6 exact completed replay"
        );
    }

    #[test]
    fn repository_publication_intent_and_receipt_fail_closed_on_every_axis() {
        // Cause/effect decision table: C1 input kind is Repository; C2 source is
        // writable; C3 source/config identities match; C4 branch is non-empty;
        // C5 commit is full 40-hex; C6 command fingerprint and provisioning
        // receipt coordinates are exact. E1 admits one intent/receipt; E2 rejects
        // before durable mutation. Each invalid rule changes one cause only.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
        // | V1 | T | T | T | T | T | T | E1 |
        // | V2 | F | - | - | - | - | - | E2 |
        // | V3 | T | F | - | - | - | - | E2 |
        // | V4 | T | T | F | - | - | - | E2 |
        // | V5 | T | T | T | F | - | - | E2 |
        // | V6 | T | T | T | T | F | - | E2 |
        // | V7 | T | T | T | T | T | F | E2 |
        let mut invalid_values = Vec::new();
        let mut file = publication_intent();
        file.input.source = crate::ResolvedInputSource::File {
            file_id: awaken_resource_contract::FileId::from("file-1"),
        };
        invalid_values.push(file);
        let mut readonly = publication_intent();
        readonly.input.access = ResourceAccess::ReadOnly;
        invalid_values.push(readonly);
        let mut mismatched_config = publication_intent();
        let crate::ResolvedInputSource::Repository { config, .. } =
            &mut mismatched_config.input.source
        else {
            unreachable!();
        };
        config.repository_id = awaken_resource_contract::RepositoryId::from("repo-2");
        invalid_values.push(mismatched_config);
        let mut blank_branch = publication_intent();
        blank_branch.expectation.branch.clear();
        invalid_values.push(blank_branch);
        let mut short_commit = publication_intent();
        short_commit.expectation.commit = "abc".into();
        invalid_values.push(short_commit);
        let mut non_hex_commit = publication_intent();
        non_hex_commit.expectation.commit = "z".repeat(40);
        invalid_values.push(non_hex_commit);

        for (index, invalid) in invalid_values.into_iter().enumerate() {
            let mut state = SessionCleanupOperation::default();
            assert!(
                matches!(
                    state.request_with_publication("invalid", invalid),
                    Err(SessionCleanupError::InvalidRepositoryPublicationIntent(_))
                ),
                "V{}",
                index + 2
            );
            assert_eq!(state, SessionCleanupOperation::NotRequested, "E2");
        }

        let mut state = SessionCleanupOperation::default();
        let intent = publication_intent();
        state
            .request_with_publication("exact", intent.clone())
            .unwrap();
        let mut changed_intent = intent;
        changed_intent.expectation.commit = "1123456789abcdef0123456789abcdef01234567".into();
        assert_eq!(
            state.request_with_publication("exact", changed_intent),
            Err(SessionCleanupError::FrozenRepositoryPublicationMismatch),
            "frozen intent cannot change"
        );
        state.freeze_targets("exact", [], 0, 0).unwrap();
        let command = state
            .publication_command("exact")
            .unwrap()
            .expect("V1 command");
        let exact = publication_receipt(&command);

        let mut mismatches = Vec::new();
        let mut command_fingerprint = exact.clone();
        command_fingerprint.command_fingerprint.push_str("-stale");
        mismatches.push(command_fingerprint);
        for axis in 0..4 {
            let mut effect = exact.effect_receipt.clone();
            match axis {
                0 => effect.repository_id.push_str("-stale"),
                1 => effect.source_remote_url.push_str("-stale"),
                2 => effect.branch.push_str("-stale"),
                3 => effect.commit.replace_range(..1, "f"),
                _ => unreachable!(),
            }
            mismatches.push(SessionRepositoryPublicationReceipt::new(&command, effect));
        }
        let mut receipt_fingerprint = exact.clone();
        receipt_fingerprint.receipt_fingerprint.push_str("-stale");
        mismatches.push(receipt_fingerprint);

        for mismatch in mismatches {
            assert_eq!(
                state.record_repository_publication_receipt("exact", mismatch),
                Err(SessionCleanupError::RepositoryPublicationReceiptMismatch),
                "V7/E2"
            );
            assert!(state.repository_publication_receipt().is_none(), "V7/E2");
        }
        assert!(
            state
                .record_repository_publication_receipt("exact", exact)
                .unwrap(),
            "V1/E1"
        );
    }

    #[test]
    fn legacy_cleanup_rows_do_not_invent_a_terminal_projection_anchor() {
        // Cause/effect decision table: C1 a legacy Requested/Completed row has
        // no Runtime cursor field; C2 a fresh never-run Session durably records
        // cursor zero. E1 legacy decode returns None so ParentTerminal remains
        // withheld; E2 fresh zero remains Some(0) across completion.
        //
        // | Rule | shape | phase | Effect |
        // | L1 | missing cursor | Requested/Completed | E1 |
        // | L2 | explicit zero | Requested/Completed | E2 |
        //
        // The distinction is required because zero is a valid Runtime
        // high-water, not a migration sentinel.
        let mut requested = SessionCleanupOperation::default();
        assert!(requested.request("never-run"));
        assert!(requested.freeze_targets("never-run", [], 0, 0).unwrap());
        assert_eq!(requested.runtime_commit_cursor(), Some(0), "L2 Requested");

        let mut legacy_requested = serde_json::to_value(&requested).unwrap();
        legacy_requested
            .as_object_mut()
            .unwrap()
            .remove("runtime_commit_cursor");
        let legacy_requested: SessionCleanupOperation =
            serde_json::from_value(legacy_requested).unwrap();
        assert_eq!(
            legacy_requested.runtime_commit_cursor(),
            None,
            "L1 Requested"
        );

        let root = requested.command_for("never-run", "never-run").unwrap();
        assert!(requested.complete("never-run", &[verified(&root)]).unwrap());
        assert_eq!(requested.runtime_commit_cursor(), Some(0), "L2 Completed");
        let mut legacy_completed = serde_json::to_value(&requested).unwrap();
        legacy_completed
            .as_object_mut()
            .unwrap()
            .remove("runtime_commit_cursor");
        let legacy_completed: SessionCleanupOperation =
            serde_json::from_value(legacy_completed).unwrap();
        assert_eq!(
            legacy_completed.runtime_commit_cursor(),
            None,
            "L1 Completed"
        );
    }

    #[test]
    fn child_threads_are_part_of_the_durable_intent_and_required_evidence() {
        let mut state = SessionCleanupOperation::default();
        state.request("session-1");
        assert!(
            state
                .freeze_targets("session-1", ["child-1".to_string()], 11, 13)
                .unwrap()
        );
        let root = state.command_for("session-1", "session-1").unwrap();
        let child = state.command_for("session-1", "child-1").unwrap();
        assert_eq!(
            state.complete("session-1", &[verified(&root)]),
            Err(SessionCleanupError::MissingReceipt)
        );
        assert!(
            state
                .complete("session-1", &[verified(&root), verified(&child),],)
                .unwrap()
        );
    }

    #[test]
    fn mismatched_or_incomplete_receipts_fail_closed() {
        let mut state = SessionCleanupOperation::default();
        state.request("session-1");
        state.freeze_targets("session-1", [], 0, 0).unwrap();
        let command = state.command_for("session-1", "session-1").unwrap();
        let mut completion = SessionCleanupCompletion::new(&command, Vec::new());
        completion.effect_id.push_str("-stale");
        assert_eq!(
            completion.verify(&command),
            Err(SessionCleanupError::ReceiptMismatch)
        );
        assert!(state.is_requested());
    }

    #[test]
    fn frozen_targets_cannot_be_expanded_by_a_late_projection() {
        let mut state = SessionCleanupOperation::default();
        assert!(state.request("session-1"));
        assert!(
            state
                .freeze_targets("session-1", ["child-1".to_string()], 19, 0)
                .unwrap()
        );
        assert!(
            !state
                .freeze_targets("session-1", ["child-1".to_string()], 19, 0)
                .unwrap()
        );
        assert_eq!(
            state.freeze_targets(
                "session-1",
                ["child-1".to_string(), "child-2".to_string()],
                20,
                0,
            ),
            Err(SessionCleanupError::FrozenTargetsMismatch)
        );
    }

    #[test]
    fn cleanup_identity_and_exact_receipt_set_follow_the_decision_table() {
        // Cause/effect graph: C1 the asserted Session matches the fenced root;
        // C2 evidence includes the root exactly once; C3 every frozen child is
        // present exactly once. Effects are E1 freeze/complete, or E2 a precise
        // fail-closed error without changing durable Requested state.
        //
        // | Rule | Session | root receipt | duplicate | child set | Effect |
        // | T05 | foreign | n/a | no | exact | OperationMismatch |
        // | T06 | exact | missing | no | child only | MissingRootReceipt |
        // | T07 | exact | present | yes | exact | DuplicateThread |
        // | T04 | exact | present | no | expanded after freeze | FrozenTargetsMismatch |
        let mut foreign = SessionCleanupOperation::default();
        foreign.request("session-a");
        assert_eq!(
            foreign.freeze_targets("session-b", [], 1, 0),
            Err(SessionCleanupError::OperationMismatch),
            "T05"
        );
        assert!(foreign.is_fenced(), "T05 leaves Session-A unchanged");

        let mut state = SessionCleanupOperation::default();
        state.request("session-a");
        state
            .freeze_targets("session-a", ["child-a".to_string()], 7, 0)
            .unwrap();
        let root = state.command_for("session-a", "session-a").unwrap();
        let child = state.command_for("session-a", "child-a").unwrap();
        let root_receipt = verified(&root);
        let child_receipt = verified(&child);
        assert_eq!(
            state.complete("session-a", std::slice::from_ref(&child_receipt)),
            Err(SessionCleanupError::MissingRootReceipt),
            "T06"
        );
        assert!(state.is_requested(), "T06");
        assert_eq!(
            state.complete(
                "session-a",
                &[root_receipt.clone(), root_receipt, child_receipt],
            ),
            Err(SessionCleanupError::DuplicateThread(
                "session-a".to_string()
            )),
            "T07"
        );
        assert!(state.is_requested(), "T07");
    }

    #[test]
    fn receipt_order_and_watermark_follow_the_decision_table() {
        // Cause/effect graph: C1 receipt arrival order varies; C2 the frozen
        // watermark is replayed exactly. Effects: E1 order is canonical; E2 a
        // different watermark cannot rewrite frozen truth. Adapter completion is
        // represented by returning this receipt, not by self-asserted booleans.
        //
        // | Rule | order | watermark | Effect |
        // | T09 | root/child or child/root | exact | same fingerprint |
        // | T10 | any | changed | FrozenTargetsMismatch |

        fn completed_with_order(reverse: bool) -> SessionCleanupOperation {
            let mut state = SessionCleanupOperation::default();
            state.request("session-order");
            state
                .freeze_targets("session-order", ["child-order".to_string()], 9, 0)
                .unwrap();
            let root = state.command_for("session-order", "session-order").unwrap();
            let child = state.command_for("session-order", "child-order").unwrap();
            let mut receipts = vec![verified(&root), verified(&child)];
            if reverse {
                receipts.reverse();
            }
            state.complete("session-order", &receipts).unwrap();
            state
        }
        let forward = completed_with_order(false);
        let reverse = completed_with_order(true);
        assert_eq!(forward, reverse, "T09 canonical receipt order");

        let mut watermark = SessionCleanupOperation::default();
        watermark.request("session-watermark");
        watermark
            .freeze_targets("session-watermark", ["child".to_string()], 10, 0)
            .unwrap();
        assert_eq!(
            watermark.freeze_targets("session-watermark", ["child".to_string()], 11, 0),
            Err(SessionCleanupError::FrozenTargetsMismatch),
            "T10"
        );
    }

    #[test]
    fn remote_completion_progress_is_durable_exact_and_replay_safe() {
        // Cause/effect graph: C1 targets are frozen; C2 a canonical completion
        // arrives for a pending target; C3 the exact completion replays; C4 a
        // conflicting completion is asserted; C5 the operation is serialized
        // between target completions. Effects: E1 remove only that command from
        // the pending projection; E2 replay is a no-op; E3 conflict fails closed;
        // E4 cold recovery retains verified progress and completes from the same
        // immutable target set.
        //
        // | Rule | target | completion | restart | Effect |
        // | R1 | frozen | canonical new | no | E1 |
        // | R2 | frozen | exact replay | no | E2 |
        // | R3 | frozen | conflicting | no | E3 |
        // | R4 | remaining | canonical | yes | E4 |
        // Constraints/invariants: the frozen target set and canonical receipt
        // identity never change across retries or process recovery.
        let mut state = SessionCleanupOperation::default();
        state.request("remote-session");
        state
            .freeze_targets("remote-session", ["remote-child".to_string()], 17, 0)
            .unwrap();
        let child = state.command_for("remote-session", "remote-child").unwrap();
        let child_completion = SessionCleanupCompletion::new(&child, Vec::new());
        assert!(
            state
                .record_completion("remote-session", child_completion.clone())
                .unwrap(),
            "R1/E1"
        );
        assert_eq!(state.pending_commands("remote-session").unwrap().len(), 1);
        assert!(
            !state
                .record_completion("remote-session", child_completion.clone())
                .unwrap(),
            "R2/E2"
        );
        let mut conflicting = child_completion;
        conflicting.receipt_fingerprint.push_str("-stale");
        assert_eq!(
            state.record_completion("remote-session", conflicting),
            Err(SessionCleanupError::ReceiptMismatch),
            "R3/E3"
        );

        let encoded = serde_json::to_vec(&state).unwrap();
        let mut recovered: SessionCleanupOperation = serde_json::from_slice(&encoded).unwrap();
        let root = recovered
            .command_for("remote-session", "remote-session")
            .unwrap();
        recovered
            .record_completion(
                "remote-session",
                SessionCleanupCompletion::new(&root, Vec::new()),
            )
            .unwrap();
        assert!(
            recovered
                .pending_commands("remote-session")
                .unwrap()
                .is_empty(),
            "R4/E4"
        );
        let receipts = recovered.recorded_receipts("remote-session").unwrap();
        assert!(recovered.complete("remote-session", &receipts).unwrap());
    }

    #[test]
    fn every_terminal_cleanup_receipt_identity_axis_is_mandatory() {
        for missing in 0..5 {
            let mut axes = [true; 5];
            axes[missing] = false;
            assert!(
                !session_cleanup_completion_admitted(axes[0], axes[1], axes[2], axes[3], axes[4],),
                "receipt axis {missing}"
            );
        }
        assert!(session_cleanup_completion_admitted(
            true, true, true, true, true
        ));
    }

    proptest! {
        #[test]
        fn random_cleanup_command_sequences_refine_the_monotonic_state_model(
            actions in proptest::collection::vec(0_u8..6, 0..80),
            watermark in any::<u64>(),
        ) {
            /* Model-based cause/effect design. Each generated action is one of:
             * C0 request, C1 exact freeze, C2 exact receipt settlement, C3
             * foreign freeze, C4 request replay, C5 mismatched freeze replay.
             * Effects/invariants: E1 state rank never decreases; E2 Completed is
             * immutable; E3 an intent exists only in Requested; E4 foreign or
             * mismatched commands never rewrite authority. Random sequences
             * cover order/replay combinations after the deterministic decision
             * table owns each individual oracle. */
            let mut state = SessionCleanupOperation::default();
            for action in actions {
                let before = state.clone();
                let before_rank = cleanup_rank(&before);
                match action {
                    0 | 4 => {
                        state.request("model-session");
                    }
                    1 => {
                        let _ = state.freeze_targets(
                            "model-session",
                            ["model-child".to_string()],
                            watermark,
                            0,
                        );
                    }
                    2 => {
                        if state.is_requested() {
                            let receipts = state
                                .thread_ids()
                                .unwrap()
                                .iter()
                                .map(|thread_id| {
                                    let command = state
                                        .command_for("model-session", thread_id)
                                        .unwrap();
                                    SessionCleanupCompletion::new(&command, Vec::new())
                                    .verify(&command)
                                    .unwrap()
                                })
                                .collect::<Vec<_>>();
                            state.complete("model-session", &receipts).unwrap();
                        } else if state.is_completed() {
                            prop_assert_eq!(state.complete("model-session", &[]), Ok(false));
                        } else {
                            prop_assert_eq!(
                                state.complete("model-session", &[]),
                                Err(SessionCleanupError::NotRequested),
                            );
                        }
                    }
                    3 => {
                        let _ = state.freeze_targets("foreign-session", [], watermark, 0);
                    }
                    5 => {
                        let _ = state.freeze_targets(
                            "model-session",
                            ["late-child".to_string()],
                            watermark.wrapping_add(1),
                            0,
                        );
                    }
                    _ => unreachable!(),
                }
                prop_assert!(cleanup_rank(&state) >= before_rank, "E1");
                if before.is_completed() {
                    prop_assert_eq!(&state, &before, "E2");
                }
                prop_assert_eq!(
                    state.command_for("model-session", "model-session").is_some(),
                    state.is_requested(),
                    "E3",
                );
            }
        }
    }

    fn cleanup_rank(state: &SessionCleanupOperation) -> u8 {
        match state {
            SessionCleanupOperation::NotRequested => 0,
            SessionCleanupOperation::Fenced { .. } => 1,
            SessionCleanupOperation::Requested { .. } => 2,
            SessionCleanupOperation::Completed { .. } => 3,
            SessionCleanupOperation::RepositoryPublication(publication) => {
                cleanup_rank(&publication.cleanup)
            }
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn session_cleanup_completion_requires_every_identity_axis() {
        let artifact_effects_unique = kani::any::<bool>();
        let session_matches = kani::any::<bool>();
        let thread_matches = kani::any::<bool>();
        let effect_matches = kani::any::<bool>();
        let canonical_receipt_matches = kani::any::<bool>();
        let admitted = session_cleanup_completion_admitted(
            artifact_effects_unique,
            session_matches,
            thread_matches,
            effect_matches,
            canonical_receipt_matches,
        );
        assert_eq!(
            admitted,
            artifact_effects_unique
                && session_matches
                && thread_matches
                && effect_matches
                && canonical_receipt_matches
        );
    }

    #[kani::proof]
    fn session_cleanup_phase_advances_only_not_requested_fenced_requested_completed() {
        let current_code = kani::any::<u8>();
        let next_code = kani::any::<u8>();
        kani::assume(current_code < 4);
        kani::assume(next_code < 4);
        let current = match current_code {
            0 => SessionCleanupPhase::NotRequested,
            1 => SessionCleanupPhase::Fenced,
            2 => SessionCleanupPhase::Requested,
            3 => SessionCleanupPhase::Completed,
            _ => unreachable!(),
        };
        let next = match next_code {
            0 => SessionCleanupPhase::NotRequested,
            1 => SessionCleanupPhase::Fenced,
            2 => SessionCleanupPhase::Requested,
            3 => SessionCleanupPhase::Completed,
            _ => unreachable!(),
        };
        assert_eq!(
            session_cleanup_phase_advance_admitted(current, next),
            next_code == current_code + 1
        );
    }
}
