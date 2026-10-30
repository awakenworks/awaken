//! Cumulative settlement through the existing Lifecycle root and real SQLite.

use super::{
    FaultingSessionRepository, RecordingEnvironmentSource, application, create, persisted,
};
use crate::SessionMutationError;
use awaken_session_contract::{LifecycleOutbox as _, SessionRepositoryReads as _};
use awaken_session_contract::{
    ManagedListPriceSnapshot, ManagedTokenListRates, SessionBudgetState, SessionModelUsage,
    SessionUsage,
};
use awaken_session_types::SessionId;
use std::collections::BTreeMap;
use std::sync::Arc;

#[tokio::test]
async fn budget_settlement_keeps_failed_and_response_lost_cas_on_the_original_root() {
    // B10 cause/effect graph: C1 the existing decorator fails before commit or
    // commits then reports Conflict; C2 the same cumulative usage is retried.
    // E1 C1-before returns the original error with identical SQLite root/outbox;
    // E2 the successful retry commits one exact usage/cost/reach generation;
    // E3 C1-after reloads that original commit without another debit or fact;
    // E4 C2 is a no-op with the same result and no additional root revision.
    // | Rule | Existing fault | First response | Retry / replay |
    // | B10a | fail_once | E1 | E2, then E4 |
    // | B10b | commit_then_conflict_once | E2 + E3 | E4 |
    // Scope: this command accepts already observed usage; the original B1/B2
    // coordination tests own committed Thread attribution and request gating.
    // No new provider, price authority, fault decorator or store is introduced.
    for rule in ["before-commit", "after-commit"] {
        let store = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let repository = Arc::new(FaultingSessionRepository::new(store.clone()));
        let id = SessionId::from(format!("budget-settlement-{rule}"));
        let mut session = persisted(id.as_str(), false, "idle");
        session.budget = SessionBudgetState::active(
            1,
            ManagedListPriceSnapshot {
                snapshot_id: "settlement-prices-v1".into(),
                version: 1,
                effective_at_unix_ms: 1,
                arithmetic_version: 1,
                model_rates: BTreeMap::from([(
                    "model".into(),
                    ManagedTokenListRates {
                        input_micros_per_million: 1_000_000,
                        ..Default::default()
                    },
                )]),
                runtime_rates: Default::default(),
                fingerprint: "settlement-prices-v1-fingerprint".into(),
            },
        );
        create(store.as_ref(), session).await;
        // The same decorator enters the existing constructor before sharing;
        // only its original one-shot fault flag is armed after construction.
        let app = application(
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let lifecycle = app.lifecycle();
        let before = store.get(&id).await.unwrap();
        let before_facts = store.pending_lifecycle().await.unwrap();
        let usage = SessionUsage {
            input_tokens: 10_000,
            by_model: BTreeMap::from([(
                "model".into(),
                SessionModelUsage {
                    input_tokens: 10_000,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let operation = "reconcile-managed-budget-usage";
        if rule == "before-commit" {
            repository.fail_once(operation);
            let error = lifecycle
                .reconcile_managed_budget_usage(&id, usage.clone())
                .await
                .unwrap_err();
            assert_eq!(
                error,
                SessionMutationError::Unavailable("injected root-CAS outage".into()),
                "B10a/E1 original typed failure"
            );
            assert_eq!(store.get(&id).await.unwrap(), before, "B10a/E1");
            assert_eq!(
                store.pending_lifecycle().await.unwrap(),
                before_facts,
                "B10a/E1 no uncommitted lifecycle fact"
            );
        } else {
            repository.commit_then_conflict_once(operation);
        }
        let settled = lifecycle
            .reconcile_managed_budget_usage(&id, usage.clone())
            .await
            .unwrap();
        let committed = store.get(&id).await.unwrap();
        assert_eq!(settled.session, committed, "{rule}: original root readback");
        assert_eq!(settled.reached_now, rule == "before-commit", "{rule}");
        assert_eq!(committed.revision.0, before.revision.0 + 1, "{rule}");
        assert!(!committed.budget.can_admit_model_request(), "{rule}");
        assert_eq!(
            committed.usage_cursor.by_model["model"].input_tokens,
            10_000
        );
        let transitions = committed.budget.reach_transitions();
        assert_eq!(transitions.len(), 1, "B10/E2+E3 {rule}");
        assert_eq!(transitions[0].generation, 1, "{rule}");
        assert_eq!(
            transitions[0].consumed_numerator,
            SessionBudgetState::MICROS_PER_MINOR_USD * SessionBudgetState::COST_DENOMINATOR,
            "B10 exact one-minor-unit cost"
        );
        let facts = store.pending_lifecycle().await.unwrap();
        assert_eq!(facts.len(), before_facts.len() + 1, "{rule}");
        let reaches = facts
            .iter()
            .filter(|fact| {
                fact.object_id == id.as_str() && fact.event_type == "session.budget_reached"
            })
            .collect::<Vec<_>>();
        assert_eq!(reaches.len(), 1, "B10 exact original outbox fact");
        assert_eq!(reaches[0].id, format!("budget-reached:{id}:1"));
        let replay = lifecycle
            .reconcile_managed_budget_usage(&id, usage)
            .await
            .unwrap();
        assert!(!replay.reached_now, "B10/E4 {rule}");
        assert_eq!(replay.session, committed, "B10/E4 {rule}");
        assert_eq!(store.get(&id).await.unwrap(), committed, "B10/E4 {rule}");
        assert_eq!(store.pending_lifecycle().await.unwrap(), facts, "B10/E4");
    }
}
