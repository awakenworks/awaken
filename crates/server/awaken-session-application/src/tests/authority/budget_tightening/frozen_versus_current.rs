//! T06-C03 (冻结与动态事实), O0 half: a Session's frozen model, Agent revision
//! and list-price snapshot keep their admitted values after the CURRENT
//! catalog moves on, through every later budget-relevant command.
//!
//! Oracles (groups JSON order):
//! - O0 frozen version, price and model identity keep their original values;
//! - O1 dynamic revocation is judged by the current authority. O1 is owned by
//!   the parent `tightened_budget_governs_the_next_model_request_and_keeps_frozen_pins`
//!   (current budget authority) and by
//!   `update::tests::credential_lifecycle_replacement_is_monotonic_and_topology_closed`
//!   (a revoked credential source may not be retained).
//!
//! Negative control: "always re-resolving the current version" must fail. It
//! is executed: the installed list-price authority now answers a v2 snapshot;
//! the provider is shown to really answer v2, and the `keeps_frozen_pins`
//! checker rejects the pins a re-resolving implementation would hold.
use super::*;
use awaken_session_contract::{ManagedListPriceError, ManagedListPriceRequest};

/// The current list-price catalog, already moved to a later snapshot.
#[derive(Default)]
struct CurrentPrices {
    calls: AtomicUsize,
}

fn current_snapshot() -> ManagedListPriceSnapshot {
    ManagedListPriceSnapshot {
        snapshot_id: "t06-c03-prices-v2".into(),
        version: 2,
        effective_at_unix_ms: 2,
        fingerprint: "t06-c03-prices-v2-fingerprint".into(),
        ..price_snapshot()
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::ManagedListPriceProvider for CurrentPrices {
    async fn resolve_snapshot(
        &self,
        _request: ManagedListPriceRequest,
    ) -> Result<ManagedListPriceSnapshot, ManagedListPriceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(current_snapshot())
    }
}

/// O0 checker over every frozen identity the Session admitted with.
fn keeps_frozen_pins(
    session: &PersistedSession,
    model: &str,
    revision: Option<u64>,
    price: &ManagedListPriceSnapshot,
) -> bool {
    let baseline = session.frozen_baseline().expect("fixture is frozen");
    baseline.execution_model_ref == model
        && baseline.agent_revision == revision
        && session.budget.price_snapshot() == Some(price)
}

#[tokio::test]
async fn frozen_pins_survive_a_moved_price_catalog_through_later_commands() {
    let id = "t06-c03-frozen";
    let run = RunId(format!("{id}-run"));
    let (repo, runtime, mut app) = fixture(&[(id, "idle")]).await;
    let prices = Arc::new(CurrentPrices::default());
    app.set_managed_list_price_provider(prices.clone());
    let admitted = repo.get(&SessionId::from(id)).await.unwrap();
    let baseline = admitted.frozen_baseline().unwrap().clone();
    let (model, price) = frozen_pins(&admitted);
    let revision = baseline.agent_revision;
    assert_eq!(price, price_snapshot(), "fixture admitted v1 prices");

    // Later commands: model-request gates, a settlement and a budget update.
    for tokens in [0, FIRST_STEP_TOKENS] {
        runtime.set_root_usage(usage(tokens));
        assert!(gate(&app, id, &run).await, "admitted at {tokens}");
    }
    Box::pin(
        app.update_commands
            .update_session(&SessionId::from(id), tighten("t06-c03-tighten")),
    )
    .await
    .expect("dynamic budget update");
    app.lifecycle()
        .reconcile_managed_budget_usage(&SessionId::from(id), usage(FIRST_STEP_TOKENS))
        .await
        .expect("settlement replay");

    let current = repo.get(&SessionId::from(id)).await.unwrap();
    assert!(
        keeps_frozen_pins(&current, &model, revision, &price),
        "O0 model, Agent revision and price keep their admitted values"
    );
    assert_eq!(
        current.frozen_baseline().unwrap(),
        &baseline,
        "O0 the whole frozen baseline is byte-exact"
    );
    assert_eq!(
        prices.calls.load(Ordering::SeqCst),
        0,
        "O0 no later command consults the current price catalog"
    );

    // NC: the current authority really answers a different version.
    let moved = awaken_session_contract::ManagedListPriceProvider::resolve_snapshot(
        prices.as_ref(),
        ManagedListPriceRequest {
            occurred_at_unix_ms: 3,
            model_refs: vec![model.clone()],
        },
    )
    .await
    .expect("current catalog");
    assert_ne!(moved, price, "NC the catalog moved on");
    assert!(
        !keeps_frozen_pins(&current, &model, revision, &moved),
        "NC pins re-resolved from the current catalog are detected"
    );
}
