//! Durable intent and receipt vocabulary for terminal Session cleanup.
//!
//! The Session aggregate owns whether cleanup is required or complete. Runtime
//! implementations own the substrate-specific effects, but must execute them
//! from the stable intent and return a receipt for that exact intent.

use awaken_resource_contract::ArtifactPublicationReceipt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The one terminal cleanup lifecycle stored by the Session aggregate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionTerminalCleanupState {
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

impl SessionTerminalCleanupState {
    /// Commit the stable whole-Session intent before any Runtime effect.
    pub fn request(&mut self, session_id: &str) -> bool {
        if !matches!(self, Self::NotRequested) {
            return false;
        }
        *self = Self::Fenced {
            effect_id: cleanup_effect_id(session_id),
        };
        true
    }

    /// Freeze the complete target set only after the terminal fence is durable
    /// and the parent Runtime has stopped. A Requested/Completed intent is
    /// immutable: recovery may replay it, but no later projection can expand it.
    pub fn freeze_targets(
        &mut self,
        session_id: &str,
        thread_ids: impl IntoIterator<Item = String>,
        delegation_watermark: u64,
    ) -> Result<bool, SessionTerminalCleanupError> {
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
                        Err(SessionTerminalCleanupError::FrozenTargetsMismatch)
                    }
                }
                Self::NotRequested => Err(SessionTerminalCleanupError::NotRequested),
                Self::Fenced { .. } => unreachable!(),
            };
        };
        if *effect_id != cleanup_effect_id(session_id) {
            return Err(SessionTerminalCleanupError::IntentMismatch);
        }
        let mut durable = BTreeSet::from([session_id.to_string()]);
        durable.extend(thread_ids);
        *self = Self::Requested {
            effect_id: effect_id.clone(),
            thread_ids: durable,
            delegation_watermark,
        };
        Ok(true)
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
    pub fn is_fenced(&self) -> bool {
        matches!(self, Self::Fenced { .. })
    }

    #[must_use]
    pub fn is_requested(&self) -> bool {
        matches!(self, Self::Requested { .. })
    }

    #[must_use]
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        matches!(self, Self::Fenced { .. } | Self::Requested { .. })
    }

    #[must_use]
    pub fn intent_for(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Option<SessionTerminalCleanupIntent> {
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
        Some(SessionTerminalCleanupIntent::new(
            session_id, thread_id, effect_id,
        ))
    }

    /// Commit receipt evidence only after every thread effect has succeeded.
    pub fn complete(
        &mut self,
        session_id: &str,
        receipts: &[SessionTerminalCleanupReceipt],
    ) -> Result<bool, SessionTerminalCleanupError> {
        let Self::Requested {
            effect_id,
            thread_ids,
            delegation_watermark,
        } = self
        else {
            return if self.is_completed() {
                Ok(false)
            } else {
                Err(SessionTerminalCleanupError::NotRequested)
            };
        };
        if receipts.is_empty() {
            return Err(SessionTerminalCleanupError::MissingReceipt);
        }
        let mut evidence = receipts.to_vec();
        evidence.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
        for pair in evidence.windows(2) {
            if pair[0].thread_id == pair[1].thread_id {
                return Err(SessionTerminalCleanupError::DuplicateThread(
                    pair[0].thread_id.clone(),
                ));
            }
        }
        let evidenced_threads = evidence
            .iter()
            .map(|receipt| receipt.thread_id.clone())
            .collect::<BTreeSet<_>>();
        if !evidenced_threads.contains(session_id) {
            return Err(SessionTerminalCleanupError::MissingRootReceipt);
        }
        if evidenced_threads != *thread_ids {
            return Err(SessionTerminalCleanupError::MissingReceipt);
        }
        for receipt in &evidence {
            let intent =
                SessionTerminalCleanupIntent::new(session_id, &receipt.thread_id, effect_id);
            receipt.verify(&intent)?;
        }
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-receipt-v1",
            effect_id.as_str(),
            *delegation_watermark,
            evidence
                .iter()
                .map(|receipt| {
                    (
                        receipt.thread_id.as_str(),
                        receipt.effect_id.as_str(),
                        receipt.receipt_fingerprint.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
        ));
        *self = Self::Completed {
            effect_id: effect_id.clone(),
            thread_ids: thread_ids.clone(),
            delegation_watermark: *delegation_watermark,
            receipt_fingerprint,
        };
        Ok(true)
    }
}

/// Stable, per-thread terminal cleanup command derived from the root intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTerminalCleanupIntent {
    pub session_id: String,
    pub thread_id: String,
    pub effect_id: String,
}

impl SessionTerminalCleanupIntent {
    /// Construct the canonical intent when no persisted state object is at hand
    /// (for example a compatibility call into `SessionRuntime::end_session`).
    #[must_use]
    pub fn for_thread(session_id: &str, thread_id: &str) -> Self {
        Self::new(session_id, thread_id, &cleanup_effect_id(session_id))
    }

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

/// Runtime evidence that the exact per-thread cleanup intent completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTerminalCleanupReceipt {
    pub session_id: String,
    pub thread_id: String,
    pub effect_id: String,
    pub artifact_receipts: Vec<ArtifactPublicationReceipt>,
    pub repositories_settled: bool,
    pub skills_settled: bool,
    pub environment_disposed: bool,
    pub receipt_fingerprint: String,
}

impl SessionTerminalCleanupReceipt {
    #[must_use]
    pub fn new(
        intent: &SessionTerminalCleanupIntent,
        mut artifact_receipts: Vec<ArtifactPublicationReceipt>,
        repositories_settled: bool,
        skills_settled: bool,
        environment_disposed: bool,
    ) -> Self {
        artifact_receipts.sort_by(|left, right| left.effect_id.cmp(&right.effect_id));
        let receipt_fingerprint = crate::stable_fingerprint(&(
            "session-terminal-cleanup-thread-receipt-v1",
            intent.session_id.as_str(),
            intent.thread_id.as_str(),
            intent.effect_id.as_str(),
            artifact_receipts
                .iter()
                .map(|receipt| (receipt.effect_id.as_str(), receipt.content_id.as_str()))
                .collect::<Vec<_>>(),
            repositories_settled,
            skills_settled,
            environment_disposed,
        ));
        Self {
            session_id: intent.session_id.clone(),
            thread_id: intent.thread_id.clone(),
            effect_id: intent.effect_id.clone(),
            artifact_receipts,
            repositories_settled,
            skills_settled,
            environment_disposed,
            receipt_fingerprint,
        }
    }

    pub fn verify(
        &self,
        intent: &SessionTerminalCleanupIntent,
    ) -> Result<(), SessionTerminalCleanupError> {
        let duplicate_artifact = self
            .artifact_receipts
            .windows(2)
            .any(|pair| pair[0].effect_id == pair[1].effect_id);
        if duplicate_artifact
            || !self.repositories_settled
            || !self.skills_settled
            || !self.environment_disposed
            || self.session_id != intent.session_id
            || self.thread_id != intent.thread_id
            || self.effect_id != intent.effect_id
            || *self
                != Self::new(
                    intent,
                    self.artifact_receipts.clone(),
                    self.repositories_settled,
                    self.skills_settled,
                    self.environment_disposed,
                )
        {
            return Err(SessionTerminalCleanupError::ReceiptMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionTerminalCleanupError {
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
    #[error("Session terminal cleanup state does not match its Session identity")]
    IntentMismatch,
    #[error("Session terminal cleanup targets were already frozen at a different watermark")]
    FrozenTargetsMismatch,
}

fn cleanup_effect_id(session_id: &str) -> String {
    crate::stable_fingerprint(&("session-terminal-cleanup-v1", session_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_and_receipt_are_stable_across_recovery() {
        let mut state = SessionTerminalCleanupState::default();
        assert!(state.request("session-1"));
        assert!(state.is_fenced());
        assert!(state.freeze_targets("session-1", [], 7).unwrap());
        let first = state.intent_for("session-1", "session-1").unwrap();
        let replay = state.intent_for("session-1", "session-1").unwrap();
        assert_eq!(first, replay);
        let receipt = SessionTerminalCleanupReceipt::new(&first, Vec::new(), true, true, true);
        assert!(state.complete("session-1", &[receipt]).unwrap());
        assert!(state.is_completed());
    }

    #[test]
    fn child_threads_are_part_of_the_durable_intent_and_required_evidence() {
        let mut state = SessionTerminalCleanupState::default();
        state.request("session-1");
        assert!(
            state
                .freeze_targets("session-1", ["child-1".to_string()], 11)
                .unwrap()
        );
        let root = state.intent_for("session-1", "session-1").unwrap();
        let child = state.intent_for("session-1", "child-1").unwrap();
        assert_eq!(
            state.complete(
                "session-1",
                &[SessionTerminalCleanupReceipt::new(
                    &root,
                    Vec::new(),
                    true,
                    true,
                    true,
                )]
            ),
            Err(SessionTerminalCleanupError::MissingReceipt)
        );
        assert!(
            state
                .complete(
                    "session-1",
                    &[
                        SessionTerminalCleanupReceipt::new(&root, Vec::new(), true, true, true,),
                        SessionTerminalCleanupReceipt::new(&child, Vec::new(), true, true, true,),
                    ],
                )
                .unwrap()
        );
    }

    #[test]
    fn mismatched_or_incomplete_receipts_fail_closed() {
        let mut state = SessionTerminalCleanupState::default();
        state.request("session-1");
        state.freeze_targets("session-1", [], 0).unwrap();
        let intent = state.intent_for("session-1", "session-1").unwrap();
        let mut receipt = SessionTerminalCleanupReceipt::new(&intent, Vec::new(), true, true, true);
        receipt.effect_id.push_str("-stale");
        assert_eq!(
            state.complete("session-1", &[receipt]),
            Err(SessionTerminalCleanupError::ReceiptMismatch)
        );
        assert!(state.is_requested());
    }

    #[test]
    fn frozen_targets_cannot_be_expanded_by_a_late_projection() {
        let mut state = SessionTerminalCleanupState::default();
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
            Err(SessionTerminalCleanupError::FrozenTargetsMismatch)
        );
    }
}
