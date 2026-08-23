//! Domain-owned operation vocabulary for terminal Session cleanup.
//!
//! The Session aggregate owns whether cleanup is required or complete. Runtime
//! implementations own the substrate-specific effects, but must execute them
//! from a stable [`SessionCleanupCommand`]. Their untrusted completion report
//! becomes a [`VerifiedSessionCleanupReceipt`] only after exact command binding.

use awaken_resource_contract::ArtifactPublicationReceipt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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
/// The serde representation intentionally retains the existing tagged shape so
/// persisted Session rows remain backward compatible across this domain rename.
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
        receipt_fingerprint: String,
    },
}

impl SessionCleanupOperation {
    #[must_use]
    pub(crate) const fn phase(&self) -> SessionCleanupPhase {
        match self {
            Self::NotRequested => SessionCleanupPhase::NotRequested,
            Self::Fenced { .. } => SessionCleanupPhase::Fenced,
            Self::Requested { .. } => SessionCleanupPhase::Requested,
            Self::Completed { .. } => SessionCleanupPhase::Completed,
        }
    }

    fn advance_to(&mut self, next: Self) -> bool {
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

    /// Freeze the complete target set only after the terminal fence is durable
    /// and the parent Runtime has stopped. A Requested/Completed intent is
    /// immutable: recovery may replay it, but no later projection can expand it.
    pub fn freeze_targets(
        &mut self,
        session_id: &str,
        thread_ids: impl IntoIterator<Item = String>,
        delegation_watermark: u64,
    ) -> Result<bool, SessionCleanupError> {
        let Self::Fenced { effect_id } = self else {
            return match self {
                Self::Requested {
                    thread_ids: durable,
                    delegation_watermark: durable_watermark,
                    ..
                }
                | Self::Completed {
                    thread_ids: durable,
                    delegation_watermark: durable_watermark,
                    ..
                } => {
                    let mut asserted = BTreeSet::from([session_id.to_string()]);
                    asserted.extend(thread_ids);
                    if *durable == asserted && *durable_watermark == delegation_watermark {
                        Ok(false)
                    } else {
                        Err(SessionCleanupError::FrozenTargetsMismatch)
                    }
                }
                Self::NotRequested => Err(SessionCleanupError::NotRequested),
                Self::Fenced { .. } => unreachable!(),
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
            completions: BTreeMap::new(),
        });
        if !advanced {
            return Err(SessionCleanupError::InvalidPhaseAdvance);
        }
        Ok(advanced)
    }

    #[must_use]
    pub fn thread_ids(&self) -> Option<&BTreeSet<String>> {
        match self {
            Self::Requested { thread_ids, .. } | Self::Completed { thread_ids, .. } => {
                Some(thread_ids)
            }
            Self::NotRequested | Self::Fenced { .. } => None,
        }
    }

    #[must_use]
    pub const fn is_fenced(&self) -> bool {
        matches!(self, Self::Fenced { .. })
    }

    #[must_use]
    pub const fn is_requested(&self) -> bool {
        matches!(self, Self::Requested { .. })
    }

    #[must_use]
    pub const fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    #[must_use]
    pub const fn needs_reconciliation(&self) -> bool {
        matches!(self, Self::Fenced { .. } | Self::Requested { .. })
    }

    #[must_use]
    pub fn command_for(&self, session_id: &str, thread_id: &str) -> Option<SessionCleanupCommand> {
        let Self::Requested {
            effect_id,
            thread_ids,
            ..
        } = self
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
        let Self::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = self
        else {
            return if self.is_completed() {
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

    /// Verify and durably retain one completion for an already-frozen target.
    /// Exact replay is a no-op; a conflicting completion fails closed.
    pub fn record_completion(
        &mut self,
        session_id: &str,
        completion: SessionCleanupCompletion,
    ) -> Result<bool, SessionCleanupError> {
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
        let Self::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = self
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
        let Self::Requested {
            effect_id,
            thread_ids,
            delegation_watermark,
            ..
        } = self
        else {
            return if self.is_completed() {
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
            if receipt.command != expected {
                return Err(SessionCleanupError::ReceiptMismatch);
            }
        }
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-receipt-v1",
            effect_id.as_str(),
            *delegation_watermark,
            evidence
                .iter()
                .map(|receipt| {
                    (
                        receipt.thread_id(),
                        receipt.effect_id(),
                        receipt.receipt_fingerprint(),
                    )
                })
                .collect::<Vec<_>>(),
        ));
        let completed_effect_id = effect_id.clone();
        let completed_thread_ids = thread_ids.clone();
        let completed_watermark = *delegation_watermark;
        let advanced = self.advance_to(Self::Completed {
            effect_id: completed_effect_id,
            thread_ids: completed_thread_ids,
            delegation_watermark: completed_watermark,
            receipt_fingerprint,
        });
        if !advanced {
            return Err(SessionCleanupError::InvalidPhaseAdvance);
        }
        Ok(advanced)
    }
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

/// Untrusted Runtime report that one per-thread cleanup command completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCleanupCompletion {
    pub session_id: String,
    pub thread_id: String,
    pub effect_id: String,
    pub artifact_receipts: Vec<ArtifactPublicationReceipt>,
    pub receipt_fingerprint: String,
}

impl SessionCleanupCompletion {
    #[must_use]
    pub fn new(
        command: &SessionCleanupCommand,
        mut artifact_receipts: Vec<ArtifactPublicationReceipt>,
    ) -> Self {
        artifact_receipts.sort_by(|left, right| left.effect_id.cmp(&right.effect_id));
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-thread-receipt-v1",
            command.session_id.as_str(),
            command.thread_id.as_str(),
            command.effect_id.as_str(),
            artifact_receipts
                .iter()
                .map(|receipt| (receipt.effect_id.as_str(), receipt.content_id.as_str()))
                .collect::<Vec<_>>(),
        ));
        Self {
            session_id: command.session_id.clone(),
            thread_id: command.thread_id.clone(),
            effect_id: command.effect_id.clone(),
            artifact_receipts,
            receipt_fingerprint,
        }
    }

    pub fn verify(
        &self,
        command: &SessionCleanupCommand,
    ) -> Result<VerifiedSessionCleanupReceipt, SessionCleanupError> {
        let duplicate_artifact = self
            .artifact_receipts
            .windows(2)
            .any(|pair| pair[0].effect_id == pair[1].effect_id);
        let canonical = Self::new(command, self.artifact_receipts.clone());
        if !session_cleanup_completion_admitted(
            !duplicate_artifact,
            self.session_id == command.session_id,
            self.thread_id == command.thread_id,
            self.effect_id == command.effect_id,
            *self == canonical,
        ) {
            return Err(SessionCleanupError::ReceiptMismatch);
        }
        Ok(VerifiedSessionCleanupReceipt {
            command: command.clone(),
            completion: self.clone(),
        })
    }
}

/// Exact, process-local completion evidence admitted against one cleanup
/// command. It is intentionally not serializable and has no public constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSessionCleanupReceipt {
    command: SessionCleanupCommand,
    completion: SessionCleanupCompletion,
}

impl VerifiedSessionCleanupReceipt {
    #[must_use]
    pub fn command(&self) -> &SessionCleanupCommand {
        &self.command
    }

    #[must_use]
    pub fn completion(&self) -> &SessionCleanupCompletion {
        &self.completion
    }

    fn thread_id(&self) -> &str {
        &self.completion.thread_id
    }

    fn effect_id(&self) -> &str {
        &self.completion.effect_id
    }

    fn receipt_fingerprint(&self) -> &str {
        &self.completion.receipt_fingerprint
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
    use proptest::prelude::*;

    fn verified(command: &SessionCleanupCommand) -> VerifiedSessionCleanupReceipt {
        SessionCleanupCompletion::new(command, Vec::new())
            .verify(command)
            .unwrap()
    }

    #[test]
    fn intent_and_receipt_are_stable_across_recovery() {
        let mut state = SessionCleanupOperation::default();
        assert!(state.request("session-1"));
        assert!(state.is_fenced());
        assert!(state.freeze_targets("session-1", [], 7).unwrap());
        let first = state.command_for("session-1", "session-1").unwrap();
        let replay = state.command_for("session-1", "session-1").unwrap();
        assert_eq!(first, replay);
        let receipt = verified(&first);
        assert!(state.complete("session-1", &[receipt]).unwrap());
        assert!(state.is_completed());
    }

    #[test]
    fn child_threads_are_part_of_the_durable_intent_and_required_evidence() {
        let mut state = SessionCleanupOperation::default();
        state.request("session-1");
        assert!(
            state
                .freeze_targets("session-1", ["child-1".to_string()], 11)
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
        state.freeze_targets("session-1", [], 0).unwrap();
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
                .freeze_targets("session-1", ["child-1".to_string()], 19)
                .unwrap()
        );
        assert!(
            !state
                .freeze_targets("session-1", ["child-1".to_string()], 19)
                .unwrap()
        );
        assert_eq!(
            state.freeze_targets(
                "session-1",
                ["child-1".to_string(), "child-2".to_string()],
                20,
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
            foreign.freeze_targets("session-b", [], 1),
            Err(SessionCleanupError::OperationMismatch),
            "T05"
        );
        assert!(foreign.is_fenced(), "T05 leaves Session-A unchanged");

        let mut state = SessionCleanupOperation::default();
        state.request("session-a");
        state
            .freeze_targets("session-a", ["child-a".to_string()], 7)
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
                .freeze_targets("session-order", ["child-order".to_string()], 9)
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
            .freeze_targets("session-watermark", ["child".to_string()], 10)
            .unwrap();
        assert_eq!(
            watermark.freeze_targets("session-watermark", ["child".to_string()], 11),
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
            .freeze_targets("remote-session", ["remote-child".to_string()], 17)
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
                        let _ = state.freeze_targets("foreign-session", [], watermark);
                    }
                    5 => {
                        let _ = state.freeze_targets(
                            "model-session",
                            ["late-child".to_string()],
                            watermark.wrapping_add(1),
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
