//! Formal (property-based) verification of the in-memory `EnvRegistry` — terminal
//! archive and fail-closed lookups (ADR-0059 verification pass). The cause-
//! effect unit test pins one sequence; these properties assert the semantics hold for ALL
//! create/archive counts and orderings the generator produces.

use awaken_env_store::InMemoryEnvRegistry;
use awaken_environment_contract::{EnvRegistry, EnvironmentConfig};
use proptest::prelude::*;

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
    r.create(
        name.into(),
        String::new(),
        Default::default(),
        EnvironmentConfig::SelfHosted,
    )
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

    /// TERMINAL ARCHIVE: the record remains retrievable for exact interpretation,
    /// but leaves `list_active` for new Session selection.
    #[test]
    fn archive_denies_new_use_and_preserves_history(k in 1usize..8) {
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
        prop_assert!(block(r.get_revision(
            a,
            awaken_environment_contract::EnvironmentRevision(1),
        )).is_some(), "authored history must remain exact-readable");
    }

    /// FAIL-CLOSED: archive/update on an id that was never created returns None.
    #[test]
    fn operations_on_a_missing_id_fail_closed(missing in "[a-z0-9_-]{1,16}") {
        let r = reg();
        // (No creates, so any id is missing.)
        prop_assert!(block(r.archive(&missing)).is_none());
        prop_assert!(block(r.update(&missing, Default::default())).is_none());
        prop_assert!(!block(r.exists(&missing)));
    }
}
