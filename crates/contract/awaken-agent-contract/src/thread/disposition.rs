//! Durable disposition of one logical Thread.
//!
//! A Thread remains the persistence aggregate.  Archiving is therefore one
//! Thread-scoped state cell committed through the ordinary [`ThreadCommit`]
//! boundary, not a protocol cache, relationship row, or child-agent aggregate.

use serde::{Deserialize, Serialize};

use crate::agent::state::{
    Command as StateCommand, MergePolicy, Scope, StateError, StateKey, Store,
};

/// Stable internal state address for the Thread disposition cell.
const THREAD_DISPOSITION_STATE_KEY: &str = "__thread_disposition";

/// Whether a logical Thread may accept more Runs.
///
/// Absence in legacy histories means [`Active`](Self::Active).  `Archived` is
/// absorbing at the command surface: this module deliberately exposes no
/// command that writes `Active` back over committed archive truth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadDisposition {
    #[default]
    Active,
    Archived,
}

struct ThreadDispositionKey;

impl StateKey for ThreadDispositionKey {
    const KEY: &'static str = THREAD_DISPOSITION_STATE_KEY;
    const SCOPE: Scope = Scope::Thread;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = ThreadDisposition;
}

/// Produce the sole public transition for the Thread disposition cell.
#[must_use]
pub fn archive_thread_command() -> StateCommand {
    ThreadDispositionKey::write(&ThreadDisposition::Archived)
}

/// Rebuild the disposition from ordinary committed Thread state.
///
/// A malformed persisted value fails closed rather than silently reviving an
/// archived Thread.
pub fn thread_disposition_from_committed_state(
    commands: &[StateCommand],
) -> Result<ThreadDisposition, StateError> {
    ThreadDispositionKey::load(&Store::rebuild(commands))
}

/// Exact commit coordinate of the absorbing archive command. A malformed or
/// cursor-less legacy prefix is not a listable archive event.
#[must_use]
pub fn archived_thread_commit_cursor(
    commands: &[StateCommand],
    state_commit_cursors: &[u64],
) -> Option<u64> {
    if commands.len() != state_commit_cursors.len() {
        return None;
    }
    commands
        .iter()
        .zip(state_commit_cursors.iter().copied())
        .rev()
        .find_map(|(command, cursor)| {
            let archived = match &command.action {
                crate::agent::state::Action::Set(value) => {
                    matches!(
                        serde_json::from_value::<ThreadDisposition>(value.clone()),
                        Ok(ThreadDisposition::Archived)
                    )
                }
                crate::agent::state::Action::Remove => false,
            };
            (command.scope == Scope::Thread
                && command.key.0 == THREAD_DISPOSITION_STATE_KEY
                && archived)
                .then_some(cursor)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_decision_table_preserves_legacy_active_and_absorbing_archive() {
        // Cause/effect graph:
        // C1=no disposition cell -> E1=legacy Thread remains Active;
        // C2=archive command committed -> E2=Thread is Archived;
        // C3=unrelated later state -> E3=archive remains authoritative.
        // Decision rules R1(C1), R2(C2), and R3(C2+C3) cover absence, the only
        // supported transition, and subsequent ordinary Thread activity.
        // Constraint/invariant: archive is the sole public, absorbing
        // disposition transition; legacy absence alone may imply Active.
        assert_eq!(
            thread_disposition_from_committed_state(&[]).unwrap(),
            ThreadDisposition::Active,
            "R1: an old Thread without the cell remains usable"
        );

        let archive = archive_thread_command();
        assert_eq!(archive.scope, Scope::Thread);
        assert_eq!(archive.merge, MergePolicy::Disjoint);
        assert_eq!(archive.key.0, THREAD_DISPOSITION_STATE_KEY);
        assert_eq!(
            thread_disposition_from_committed_state(std::slice::from_ref(&archive)).unwrap(),
            ThreadDisposition::Archived,
            "R2: the archive transition is durable Thread truth"
        );

        let unrelated = StateCommand::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            "unrelated",
            serde_json::json!({"value": 1}),
        );
        assert_eq!(
            thread_disposition_from_committed_state(&[archive, unrelated]).unwrap(),
            ThreadDisposition::Archived,
            "R3: later unrelated state cannot revive the Thread"
        );
    }

    #[test]
    fn malformed_disposition_fails_closed() {
        // Cause C1=a persisted value violates the stable enum schema.  Effect
        // E1=typed recovery returns an error; it must never default to Active.
        // One rule is sufficient because malformed-vs-absent is the complete
        // partition and the absent rule is covered by the decision table above.
        // Constraint/invariant: only absence receives the legacy Active
        // default; present but undecodable committed state fails closed.
        let malformed = StateCommand::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            THREAD_DISPOSITION_STATE_KEY,
            serde_json::json!({"unknown": true}),
        );
        assert!(
            thread_disposition_from_committed_state(&[malformed]).is_err(),
            "schema drift must not revive a possibly archived Thread"
        );
    }
}
