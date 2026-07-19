//! Typed agent state: keys, scopes, merge policies, staged commands, and the
//! materialized store rebuilt from committed commands for replay (G1/G13).
//!
//! There is deliberately no last-write-wins default. A key declares how
//! concurrent writes reconcile (`MergePolicy`), so parallel tool/hook writes are
//! only ever merged when the key says they may be, and an `Exclusive` key that
//! is written twice in one commit batch is a fail-closed conflict.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Typed state address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key(pub String);

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
    /// Concrete owner stamped by `ThreadCommit::assemble` for Run-scoped state.
    /// Older commands omit it and are handled only by explicit legacy readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<crate::agent::run::Id>,
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
            run_id: None,
            action: Action::Set(value),
        }
    }

    pub fn remove(scope: Scope, merge: MergePolicy, key: impl Into<String>) -> Self {
        Self {
            key: Key(key.into()),
            scope,
            merge,
            run_id: None,
            action: Action::Remove,
        }
    }

    /// Bind abstract Run scope to the Run whose commit carries this command.
    pub fn bind_run(&mut self, run_id: &crate::agent::run::Id) {
        if self.scope == Scope::Run {
            self.run_id = Some(run_id.clone());
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

/// A typed read failed because a present value did not match the key's declared
/// shape (schema drift on a persisted run). An *absent* key is never an error —
/// it is the key's `Default`. Surfaced instead of silently resetting to the
/// default, so a shape drift fails closed rather than discarding committed truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError {
    pub key: String,
    pub scope: Scope,
    pub detail: String,
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "state key {:?} in scope {:?} did not match its declared shape: {}",
            self.key, self.scope, self.detail
        )
    }
}

impl std::error::Error for StateError {}

/// A typed view over one `(scope, key)` cell of the untyped store: a key declares
/// its address, scope, merge policy, and value type. Reads are typed and fail
/// closed on a shape drift; writes serialize the whole value into one `Command`.
/// Promoted into the contract (ADR-0055) so the engine and every extension share
/// one typed-state discipline.
///
/// This is the base a *write-only* key needs — one that computes a whole value
/// and writes it (recall/compaction context, thread usage). A key that instead
/// reads-folds-writes a typed delta additionally implements [`FoldStateKey`], so
/// a write-only key is never forced to declare a fold it does not have (interface
/// segregation).
pub trait StateKey {
    /// Stable string address. Part of the persisted wire — never rename without
    /// a migration.
    const KEY: &'static str;
    const SCOPE: Scope;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value: Serialize + DeserializeOwned + Default;

    /// This key's untyped address.
    fn address() -> Key {
        Key(Self::KEY.to_string())
    }

    /// Fail-closed typed read. An absent key yields the `Default` (a valid
    /// initial state); a present value that does not deserialize is a drift
    /// error, never a silent reset.
    fn load(store: &Store) -> Result<Self::Value, StateError> {
        match store.get(Self::SCOPE, &Self::address()) {
            None => Ok(Self::Value::default()),
            Some(value) => serde_json::from_value(value.clone()).map_err(|error| StateError {
                key: Self::KEY.to_string(),
                scope: Self::SCOPE,
                detail: error.to_string(),
            }),
        }
    }

    /// Lenient read: an absent key *or* a drift both yield the `Default`. Prefer
    /// [`StateKey::load`]; use this only where a consumer deliberately tolerates
    /// a shape drift by resetting to the default (naming the leniency at the call
    /// site instead of hiding it inside `load`).
    fn load_or_default(store: &Store) -> Self::Value {
        Self::load(store).unwrap_or_default()
    }

    /// Produce a whole-value `Command` from an already-computed value.
    fn write(value: &Self::Value) -> Command {
        Command::set(
            Self::SCOPE,
            Self::MERGE,
            Self::KEY,
            serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        )
    }
}

/// A [`StateKey`] whose value is maintained by folding a typed delta rather than
/// written whole (the state-machine cells, thread-usage accumulation). `apply`
/// must stay total and deterministic: `Store::rebuild` replays committed updates
/// without re-validating, so an illegal update is recorded (e.g. a bounded
/// violation log), never a panic.
pub trait FoldStateKey: StateKey {
    type Update;

    /// Fold one typed update into the value.
    fn apply(value: &mut Self::Value, update: Self::Update);

    /// Load (fail-closed), fold the update, and produce a whole-value `Command`.
    fn commit(store: &Store, update: Self::Update) -> Result<Command, StateError> {
        let mut value = Self::load(store)?;
        Self::apply(&mut value, update);
        Ok(Self::write(&value))
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
    fn commutative_replaces_when_existing_is_not_an_object() {
        // A commutative shallow-merge only applies object-into-object; a scalar
        // sitting in the slot is replaced wholesale by an incoming object.
        let store = Store::rebuild(&[
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "k",
                serde_json::json!(1),
            ),
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "k",
                serde_json::json!({"a": 1}),
            ),
        ]);
        assert_eq!(
            store.get(Scope::Thread, &Key("k".into())),
            Some(&serde_json::json!({"a": 1}))
        );
    }

    #[test]
    fn commutative_replaces_when_incoming_is_not_an_object() {
        // Object sitting in the slot, scalar incoming: no merge, wholesale replace.
        let store = Store::rebuild(&[
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "k",
                serde_json::json!({"a": 1}),
            ),
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "k",
                serde_json::json!(9),
            ),
        ]);
        assert_eq!(
            store.get(Scope::Thread, &Key("k".into())),
            Some(&serde_json::json!(9))
        );
    }

    #[test]
    fn removing_an_absent_key_is_a_noop() {
        let mut store = Store::new();
        store.apply(&Command::remove(Scope::Run, MergePolicy::Disjoint, "ghost"));
        assert!(store.is_empty());
    }

    #[test]
    fn validate_batch_exclusive_remove_repeats_are_allowed() {
        // Only repeated *Set* of an exclusive key conflicts; repeated Remove does not.
        let batch = vec![
            Command::remove(Scope::Run, MergePolicy::Exclusive, "lock"),
            Command::remove(Scope::Run, MergePolicy::Exclusive, "lock"),
        ];
        assert!(validate_batch(&batch).is_ok());
    }

    #[test]
    fn validate_batch_exclusive_same_key_different_scope_is_allowed() {
        // The conflict key is (scope, key); a different scope is a different slot.
        let batch = vec![
            Command::set(
                Scope::Run,
                MergePolicy::Exclusive,
                "once",
                serde_json::json!(1),
            ),
            Command::set(
                Scope::Thread,
                MergePolicy::Exclusive,
                "once",
                serde_json::json!(2),
            ),
        ];
        assert!(validate_batch(&batch).is_ok());
    }

    #[test]
    fn state_error_display_names_key_scope_and_detail() {
        let err = StateError {
            key: "counter".into(),
            scope: Scope::Run,
            detail: "expected u64".into(),
        };
        let text = err.to_string();
        assert!(text.contains("counter"));
        assert!(text.contains("Run"));
        assert!(text.contains("expected u64"));
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

    struct Counter;
    impl StateKey for Counter {
        const KEY: &'static str = "counter";
        const SCOPE: Scope = Scope::Run;
        const MERGE: MergePolicy = MergePolicy::Disjoint;
        type Value = u64;
    }
    impl FoldStateKey for Counter {
        type Update = u64;
        fn apply(value: &mut u64, update: u64) {
            *value += update;
        }
    }

    #[test]
    fn typed_load_absent_is_default_not_error() {
        assert_eq!(Counter::load(&Store::new()), Ok(0));
    }

    #[test]
    fn typed_commit_round_trips_and_folds() {
        let mut store = Store::new();
        store.apply(&Counter::commit(&store, 2).unwrap());
        store.apply(&Counter::commit(&store, 3).unwrap());
        assert_eq!(Counter::load(&store), Ok(5));
    }

    #[test]
    fn typed_load_fails_closed_on_shape_drift() {
        // A persisted value of the wrong shape must fail closed, never silently
        // reset to the default (ADR-0055).
        let mut store = Store::new();
        store.apply(&Command::set(
            Scope::Run,
            MergePolicy::Disjoint,
            "counter",
            serde_json::json!("not a number"),
        ));
        let err = Counter::load(&store).expect_err("shape drift must fail closed");
        assert_eq!(err.key, "counter");
        assert_eq!(err.scope, Scope::Run);
        // The lenient reader deliberately tolerates the drift by defaulting.
        assert_eq!(Counter::load_or_default(&store), 0);
    }
}
