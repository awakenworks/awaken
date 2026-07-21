//! Formal (property-based) verification of the tenant-isolation fence — the security
//! invariant behind `ScopedSessionRepo` (ADR-0051 / ADR-0059 verification pass).
//!
//! The cause-effect unit tests pin specific rows; these `proptest` properties assert the
//! fence holds for ALL scope/id/session shapes the generators produce, including the
//! adversarial ones a hand-written test would miss — in particular scope/id pairs that
//! WOULD collide if the store keyed by a concatenated string instead of a tuple
//! (`("a","bc")` vs `("ab","c")`). Every property is universally quantified over the
//! generated space and checked over hundreds of cases per run.

use awaken_session_contract::{PersistedSession, ScopedSessionStore};
use awaken_session_store::InMemoryScopedSessionStore;
use awaken_tenancy::ScopeId;
use proptest::prelude::*;

/// Drive the store's async ops to completion on a current-thread runtime (the ops are a
/// `Mutex<BTreeMap>` under the hood, so they never yield). Keeps the property bodies free
/// of async plumbing without an `unsafe` hand-rolled executor (the workspace forbids it).
fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

fn session(id: &str, title: &str) -> PersistedSession {
    PersistedSession {
        session_id: id.to_string(),
        agent_id: "assistant".into(),
        model: "kimi".into(),
        title: Some(title.to_string()),
        metadata: Default::default(),
        environment_id: "env".into(),
        mcp_servers: Vec::new(),
        resources: Default::default(),
        status: "idle".into(),
        archived_at: None,
    }
}

proptest! {
    /// FENCE: a row written under one scope is invisible under any DIFFERENT scope, for
    /// every id — the core tenant-isolation guarantee. `scope_a != scope_b` is the only
    /// precondition; ids and payloads are free.
    #[test]
    fn a_row_is_never_visible_across_a_different_scope(
        scope_a in "[a-z0-9_-]{0,12}",
        scope_b in "[a-z0-9_-]{0,12}",
        id in "[a-z0-9_-]{0,12}",
    ) {
        prop_assume!(scope_a != scope_b);
        let store = InMemoryScopedSessionStore::new();
        block(store.save_scoped(&ScopeId::from(scope_a.as_str()), session(&id, "secret")));
        // The other tenant sees nothing for the same id.
        let leaked = block(store.get_scoped(&ScopeId::from(scope_b.as_str()), &id));
        prop_assert!(leaked.is_none(), "scope {scope_b:?} saw scope {scope_a:?}'s row for id {id:?}");
    }

    /// ROUND-TRIP: a row is readable back within its own scope, byte-for-byte.
    #[test]
    fn a_scope_reads_back_its_own_write(
        scope in "[a-z0-9_-]{0,12}",
        id in "[a-z0-9_-]{0,12}",
        title in "[^\\x00]{0,20}",
    ) {
        let store = InMemoryScopedSessionStore::new();
        let want = session(&id, &title);
        block(store.save_scoped(&ScopeId::from(scope.as_str()), want.clone()));
        let got = block(store.get_scoped(&ScopeId::from(scope.as_str()), &id));
        prop_assert_eq!(got, Some(want));
    }

    /// NO TUPLE-KEY COLLISION: the store keys by `(scope, id)` as a tuple, so two rows
    /// whose (scope,id) pairs are distinct AS PAIRS never clobber each other — even when
    /// their naive concatenations `scope+id` are equal (`"a"+"bc" == "ab"+"c"`). This is
    /// the property that would FAIL a separator-less string-key implementation.
    #[test]
    fn distinct_scope_id_pairs_never_clobber(
        a1 in "[a-z]{1,6}", a2 in "[a-z]{1,6}",
        b1 in "[a-z]{1,6}", b2 in "[a-z]{1,6}",
    ) {
        // Two pairs that are distinct as pairs but may share a concatenation.
        prop_assume!((a1.clone(), a2.clone()) != (b1.clone(), b2.clone()));
        let store = InMemoryScopedSessionStore::new();
        block(store.save_scoped(&ScopeId::from(a1.as_str()), session(&a2, "A")));
        block(store.save_scoped(&ScopeId::from(b1.as_str()), session(&b2, "B")));
        let got_a = block(store.get_scoped(&ScopeId::from(a1.as_str()), &a2)).map(|s| s.title);
        let got_b = block(store.get_scoped(&ScopeId::from(b1.as_str()), &b2)).map(|s| s.title);
        prop_assert_eq!(got_a, Some(Some("A".to_string())), "pair A was clobbered");
        prop_assert_eq!(got_b, Some(Some("B".to_string())), "pair B was clobbered");
    }

    /// LAST-WRITE-WINS within a scope+id: idempotent upsert, no accumulation.
    #[test]
    fn same_scope_and_id_upserts(
        scope in "[a-z0-9_-]{0,12}",
        id in "[a-z0-9_-]{0,12}",
        t1 in "[a-z]{1,10}", t2 in "[a-z]{1,10}",
    ) {
        let store = InMemoryScopedSessionStore::new();
        let s = ScopeId::from(scope.as_str());
        block(store.save_scoped(&s, session(&id, &t1)));
        block(store.save_scoped(&s, session(&id, &t2)));
        let got = block(store.get_scoped(&s, &id)).and_then(|s| s.title);
        prop_assert_eq!(got, Some(t2));
    }
}
