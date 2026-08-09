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
    use proptest::prelude::*;

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

    #[test]
    fn cleanup_identity_and_exact_receipt_set_follow_the_decision_table() {
        // Cause/effect graph: C1 the asserted Session matches the fenced root;
        // C2 evidence includes the root exactly once; C3 every frozen child is
        // present exactly once. Effects are E1 freeze/complete, or E2 a precise
        // fail-closed error without changing durable Requested state.
        //
        // | Rule | Session | root receipt | duplicate | child set | Effect |
        // | T05 | foreign | n/a | no | exact | IntentMismatch |
        // | T06 | exact | missing | no | child only | MissingRootReceipt |
        // | T07 | exact | present | yes | exact | DuplicateThread |
        // | T04 | exact | present | no | expanded after freeze | FrozenTargetsMismatch |
        let mut foreign = SessionTerminalCleanupState::default();
        foreign.request("session-a");
        assert_eq!(
            foreign.freeze_targets("session-b", [], 1),
            Err(SessionTerminalCleanupError::IntentMismatch),
            "T05"
        );
        assert!(foreign.is_fenced(), "T05 leaves Session-A unchanged");

        let mut state = SessionTerminalCleanupState::default();
        state.request("session-a");
        state
            .freeze_targets("session-a", ["child-a".to_string()], 7)
            .unwrap();
        let root = state.intent_for("session-a", "session-a").unwrap();
        let child = state.intent_for("session-a", "child-a").unwrap();
        let root_receipt = SessionTerminalCleanupReceipt::new(&root, Vec::new(), true, true, true);
        let child_receipt =
            SessionTerminalCleanupReceipt::new(&child, Vec::new(), true, true, true);
        assert_eq!(
            state.complete("session-a", std::slice::from_ref(&child_receipt)),
            Err(SessionTerminalCleanupError::MissingRootReceipt),
            "T06"
        );
        assert!(state.is_requested(), "T06");
        assert_eq!(
            state.complete(
                "session-a",
                &[root_receipt.clone(), root_receipt, child_receipt],
            ),
            Err(SessionTerminalCleanupError::DuplicateThread(
                "session-a".to_string()
            )),
            "T07"
        );
        assert!(state.is_requested(), "T07");
    }

    #[test]
    fn receipt_settlement_order_and_watermark_follow_the_decision_table() {
        // Cause/effect graph: C1 Repository/Skill/Environment evidence is all
        // true; C2 receipt arrival order varies; C3 the frozen watermark is
        // replayed exactly. Effects: E1 any false evidence is rejected; E2 order
        // is canonical; E3 a different watermark cannot rewrite frozen truth.
        //
        // | Rule | settlement | order | watermark | Effect |
        // | T08a-c | one false | any | exact | ReceiptMismatch |
        // | T09 | all true | root/child or child/root | exact | same fingerprint |
        // | T10 | all true | any | changed | FrozenTargetsMismatch |
        for flags in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            let mut state = SessionTerminalCleanupState::default();
            state.request("session-flags");
            state.freeze_targets("session-flags", [], 3).unwrap();
            let intent = state.intent_for("session-flags", "session-flags").unwrap();
            let receipt =
                SessionTerminalCleanupReceipt::new(&intent, Vec::new(), flags.0, flags.1, flags.2);
            assert_eq!(
                state.complete("session-flags", &[receipt]),
                Err(SessionTerminalCleanupError::ReceiptMismatch),
                "T08 {flags:?}"
            );
            assert!(state.is_requested(), "T08 {flags:?}");
        }

        fn completed_with_order(reverse: bool) -> SessionTerminalCleanupState {
            let mut state = SessionTerminalCleanupState::default();
            state.request("session-order");
            state
                .freeze_targets("session-order", ["child-order".to_string()], 9)
                .unwrap();
            let root = state.intent_for("session-order", "session-order").unwrap();
            let child = state.intent_for("session-order", "child-order").unwrap();
            let mut receipts = vec![
                SessionTerminalCleanupReceipt::new(&root, Vec::new(), true, true, true),
                SessionTerminalCleanupReceipt::new(&child, Vec::new(), true, true, true),
            ];
            if reverse {
                receipts.reverse();
            }
            state.complete("session-order", &receipts).unwrap();
            state
        }
        let forward = completed_with_order(false);
        let reverse = completed_with_order(true);
        assert_eq!(forward, reverse, "T09 canonical receipt order");

        let mut watermark = SessionTerminalCleanupState::default();
        watermark.request("session-watermark");
        watermark
            .freeze_targets("session-watermark", ["child".to_string()], 10)
            .unwrap();
        assert_eq!(
            watermark.freeze_targets("session-watermark", ["child".to_string()], 11),
            Err(SessionTerminalCleanupError::FrozenTargetsMismatch),
            "T10"
        );
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
            let mut state = SessionTerminalCleanupState::default();
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
                                    let intent = state
                                        .intent_for("model-session", thread_id)
                                        .unwrap();
                                    SessionTerminalCleanupReceipt::new(
                                        &intent,
                                        Vec::new(),
                                        true,
                                        true,
                                        true,
                                    )
                                })
                                .collect::<Vec<_>>();
                            state.complete("model-session", &receipts).unwrap();
                        } else if state.is_completed() {
                            prop_assert_eq!(state.complete("model-session", &[]), Ok(false));
                        } else {
                            prop_assert_eq!(
                                state.complete("model-session", &[]),
                                Err(SessionTerminalCleanupError::NotRequested),
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
                    state.intent_for("model-session", "model-session").is_some(),
                    state.is_requested(),
                    "E3",
                );
            }
        }
    }

    fn cleanup_rank(state: &SessionTerminalCleanupState) -> u8 {
        match state {
            SessionTerminalCleanupState::NotRequested => 0,
            SessionTerminalCleanupState::Fenced { .. } => 1,
            SessionTerminalCleanupState::Requested { .. } => 2,
            SessionTerminalCleanupState::Completed { .. } => 3,
        }
    }
}
