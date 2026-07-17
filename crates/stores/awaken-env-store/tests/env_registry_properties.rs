//! Formal (property-based) verification of the in-memory `EnvRegistry` — the archive-vs-
//! delete distinction and fail-closed lookups (ADR-0059 verification pass). The cause-
//! effect unit test pins one sequence; these properties assert the semantics hold for ALL
//! create/archive/delete counts and orderings the generator produces.

use awaken_env_store::InMemoryEnvRegistry;
use awaken_session_contract::env_registry::EnvRegistry;
use proptest::prelude::*;
use serde_json::json;

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

fn reg() -> InMemoryEnvRegistry {
    InMemoryEnvRegistry::new()
}

async fn make(r: &InMemoryEnvRegistry, name: &str) -> String {
    r.create(name.into(), String::new(), Default::default(), json!({}))
        .await
        .id
}

proptest! {
    /// UNIQUE IDS: `n` creates yield `n` DISTINCT ids, every one retrievable — no id reuse
    /// or collision regardless of (identical) names.
    #[test]
    fn creates_yield_distinct_retrievable_ids(n in 1usize..20) {
        let r = reg();
        let ids: Vec<String> = block(async {
            let mut v = Vec::new();
            for _ in 0..n { v.push(make(&r, "same-name").await); }
            v
        });
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        prop_assert_eq!(unique.len(), n, "ids collided");
        prop_assert!(ids.iter().all(|id| block(r.exists(id))), "a created id is missing");
    }

    /// ARCHIVE IS SOFT: after archive, the record is STILL retrievable by `get`, but leaves
    /// `list_active`. Delete is HARD: after delete, `get` returns None. The two never blur.
    #[test]
    fn archive_is_soft_and_delete_is_hard(k in 1usize..8) {
        let r = reg();
        let ids: Vec<String> = block(async {
            let mut v = Vec::new();
            for i in 0..k { v.push(make(&r, &format!("e{i}")).await); }
            v
        });
        // Archive the first: soft — get still Some, dropped from active.
        let a = &ids[0];
        prop_assert!(block(r.archive(a)).is_some());
        prop_assert!(block(r.get(a)).is_some(), "archive must keep the record retrievable");
        let active_ids: Vec<String> = block(r.list_active()).into_iter().map(|e| e.id).collect();
        prop_assert!(!active_ids.contains(a), "archived record must leave list_active");
        // Delete the last: hard — get None afterwards.
        let d = &ids[k - 1];
        prop_assert!(block(r.delete(d)), "delete of an existing id reports true");
        prop_assert!(block(r.get(d)).is_none(), "delete must remove the record");
    }

    /// FAIL-CLOSED: archive/update/delete on an id that was never created returns
    /// None/false — never a fabricated record.
    #[test]
    fn operations_on_a_missing_id_fail_closed(missing in "[a-z0-9_-]{1,16}") {
        let r = reg();
        // (No creates, so any id is missing.)
        prop_assert!(block(r.archive(&missing)).is_none());
        prop_assert!(block(r.update(&missing, Default::default())).is_none());
        prop_assert!(!block(r.delete(&missing)));
        prop_assert!(!block(r.exists(&missing)));
    }

    /// DELETE IS IDEMPOTENT: the second delete of the same id reports false (it is gone),
    /// and never resurrects anything.
    #[test]
    fn delete_is_idempotent(names in proptest::collection::vec("[a-z]{1,6}", 1..6)) {
        let r = reg();
        let id = block(make(&r, &names[0]));
        prop_assert!(block(r.delete(&id)), "first delete true");
        prop_assert!(!block(r.delete(&id)), "second delete false");
        prop_assert!(block(r.get(&id)).is_none());
    }
}
