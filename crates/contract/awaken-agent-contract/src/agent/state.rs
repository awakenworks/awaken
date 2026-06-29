//! Typed agent state: keys, scopes, merge policies, staged commands, and the
//! materialized store rebuilt from committed commands for replay (G1/G13).
//!
//! There is deliberately no last-write-wins default. A key declares how
//! concurrent writes reconcile (`MergePolicy`), so parallel tool/hook writes are
//! only ever merged when the key says they may be, and an `Exclusive` key that
//! is written twice in one commit batch is a fail-closed conflict.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Typed state address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key(pub String);

/// A materialized state entry (key plus its current JSON value).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Value {
    pub key: Key,
    pub value: serde_json::Value,
}

/// Declaration scope for a state key. The abstract scope (not a concrete id)
/// keeps a command pure data; binding to a run/thread happens at commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Scope {
    Run,
    Thread,
    Shared,
    Profile,
}

/// How concurrent writes to one `(scope, key)` reconcile within a commit batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergePolicy {
    /// Keys are expected to be written by at most one producer; a later write
    /// replaces the value.
    Disjoint,
    /// Object values shallow-merge; non-object values replace.
    Commutative,
    /// Writing the same key more than once in one batch is a conflict.
    Exclusive,
}

/// One staged state transition. It carries the key's declared scope and merge
/// policy so the commit boundary can enforce them without a separate registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub key: Key,
    pub scope: Scope,
    pub merge: MergePolicy,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Set(serde_json::Value),
    Remove,
}

impl Command {
    pub fn set(
        scope: Scope,
        merge: MergePolicy,
        key: impl Into<String>,
        value: serde_json::Value,
    ) -> Self {
        Self {
            key: Key(key.into()),
            scope,
            merge,
            action: Action::Set(value),
        }
    }

    pub fn remove(scope: Scope, merge: MergePolicy, key: impl Into<String>) -> Self {
        Self {
            key: Key(key.into()),
            scope,
            merge,
            action: Action::Remove,
        }
    }
}

/// An `Exclusive` key written more than once in one commit batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub scope: Scope,
    pub key: Key,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exclusive state key {:?} in scope {:?} written more than once in one commit",
            self.key, self.scope
        )
    }
}

/// Reject a batch where an `Exclusive` `(scope, key)` is set more than once.
/// Other policies may legitimately repeat; this is the only fail-closed rule.
pub fn validate_batch(commands: &[Command]) -> Result<(), Conflict> {
    let mut exclusive_sets: BTreeMap<(Scope, Key), usize> = BTreeMap::new();
    for command in commands {
        if command.merge == MergePolicy::Exclusive && matches!(command.action, Action::Set(_)) {
            let counter = exclusive_sets
                .entry((command.scope, command.key.clone()))
                .or_insert(0);
            *counter += 1;
            if *counter > 1 {
                return Conflict {
                    scope: command.scope,
                    key: command.key.clone(),
                }
                .into();
            }
        }
    }
    Ok(())
}

impl From<Conflict> for Result<(), Conflict> {
    fn from(conflict: Conflict) -> Self {
        Err(conflict)
    }
}

/// Materialized state grouped by `(scope, key)`. The live store is never durable
/// truth; it is rebuilt from committed commands for replay/projection (G1/G13).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Store {
    entries: BTreeMap<(Scope, Key), serde_json::Value>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, scope: Scope, key: &Key) -> Option<&serde_json::Value> {
        self.entries.get(&(scope, key.clone()))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Apply one command in sequence. `Commutative` object values shallow-merge;
    /// every other case replaces. `Remove` clears the entry.
    pub fn apply(&mut self, command: &Command) {
        let slot = (command.scope, command.key.clone());
        match &command.action {
            Action::Remove => {
                self.entries.remove(&slot);
            }
            Action::Set(value) => {
                if command.merge == MergePolicy::Commutative
                    && let Some(existing) = self.entries.get_mut(&slot)
                    && let Some(existing_obj) = existing.as_object_mut()
                    && let Some(incoming) = value.as_object()
                {
                    for (k, v) in incoming {
                        existing_obj.insert(k.clone(), v.clone());
                    }
                    return;
                }
                self.entries.insert(slot, value.clone());
            }
        }
    }

    /// Rebuild the store by replaying committed commands in order. Committed
    /// history is sequential, so it is never re-validated for conflicts.
    pub fn rebuild(commands: &[Command]) -> Self {
        let mut store = Self::new();
        for command in commands {
            store.apply(command);
        }
        store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_replays_set_and_remove_in_order() {
        let commands = vec![
            Command::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                "a",
                serde_json::json!(1),
            ),
            Command::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                "b",
                serde_json::json!(2),
            ),
            Command::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                "a",
                serde_json::json!(3),
            ),
            Command::remove(Scope::Thread, MergePolicy::Disjoint, "b"),
        ];
        let store = Store::rebuild(&commands);
        assert_eq!(
            store.get(Scope::Thread, &Key("a".into())),
            Some(&serde_json::json!(3))
        );
        assert_eq!(store.get(Scope::Thread, &Key("b".into())), None);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn scope_separates_identical_keys() {
        let commands = vec![
            Command::set(
                Scope::Run,
                MergePolicy::Disjoint,
                "k",
                serde_json::json!("run"),
            ),
            Command::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                "k",
                serde_json::json!("thread"),
            ),
        ];
        let store = Store::rebuild(&commands);
        assert_eq!(
            store.get(Scope::Run, &Key("k".into())),
            Some(&serde_json::json!("run"))
        );
        assert_eq!(
            store.get(Scope::Thread, &Key("k".into())),
            Some(&serde_json::json!("thread"))
        );
    }

    #[test]
    fn commutative_objects_shallow_merge() {
        let commands = vec![
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "obj",
                serde_json::json!({"a": 1}),
            ),
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "obj",
                serde_json::json!({"b": 2}),
            ),
        ];
        let store = Store::rebuild(&commands);
        assert_eq!(
            store.get(Scope::Thread, &Key("obj".into())),
            Some(&serde_json::json!({"a": 1, "b": 2}))
        );
    }

    #[test]
    fn commutative_non_object_replaces() {
        let commands = vec![
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "x",
                serde_json::json!(1),
            ),
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "x",
                serde_json::json!(2),
            ),
        ];
        let store = Store::rebuild(&commands);
        assert_eq!(
            store.get(Scope::Thread, &Key("x".into())),
            Some(&serde_json::json!(2))
        );
    }

    #[test]
    fn validate_batch_rejects_double_exclusive_set() {
        let batch = vec![
            Command::set(
                Scope::Run,
                MergePolicy::Exclusive,
                "lock",
                serde_json::json!(1),
            ),
            Command::set(
                Scope::Run,
                MergePolicy::Exclusive,
                "lock",
                serde_json::json!(2),
            ),
        ];
        let err = validate_batch(&batch).expect_err("double exclusive set must conflict");
        assert_eq!(err.key, Key("lock".into()));
        assert_eq!(err.scope, Scope::Run);
        assert!(err.to_string().contains("exclusive"));
    }

    #[test]
    fn validate_batch_allows_commutative_repeats_and_single_exclusive() {
        let batch = vec![
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "c",
                serde_json::json!({"a": 1}),
            ),
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "c",
                serde_json::json!({"b": 2}),
            ),
            Command::set(
                Scope::Run,
                MergePolicy::Exclusive,
                "once",
                serde_json::json!(1),
            ),
        ];
        assert!(validate_batch(&batch).is_ok());
    }

    #[test]
    fn empty_store_is_empty() {
        assert!(Store::new().is_empty());
    }
}
