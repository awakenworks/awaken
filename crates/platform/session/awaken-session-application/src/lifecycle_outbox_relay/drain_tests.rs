//! Controlled claim outcomes over real SQLite lifecycle facts during shutdown.
//! PostgreSQL store conformance owns qualified backend lease fencing.

use super::{CancellationToken, LifecycleOutboxRelay};
use awaken_session_contract::{
    LifecycleFactDelivery, LifecycleOutbox, LifecycleOutboxBatch, LifecycleOutboxCursor,
    ManagedLifecycleFact, SessionRepositoryError,
};
use awaken_session_store::SqliteManagedSessionRepository;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

struct ObservedOutbox {
    repository: SqliteManagedSessionRepository,
    claims: Mutex<Vec<(String, String)>>,
    renewals: Mutex<Vec<(String, String, bool)>>,
    finish_attempts: Mutex<Vec<(String, String, bool)>>,
    renewed: Notify,
    renewal_current: AtomicBool,
    finish_current: AtomicBool,
}

#[cfg(test)]
#[async_trait::async_trait]
impl LifecycleOutbox for ObservedOutbox {
    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError> {
        self.repository.append_lifecycle(fact).await
    }

    async fn list_pending(
        &self,
        after: Option<&LifecycleOutboxCursor>,
        limit: NonZeroU32,
    ) -> Result<LifecycleOutboxBatch, SessionRepositoryError> {
        self.repository.list_pending(after, limit).await
    }

    async fn claim_lifecycle(
        &self,
        fact_id: &str,
        ttl: Duration,
    ) -> Result<Option<String>, SessionRepositoryError> {
        if self
            .repository
            .claim_lifecycle(fact_id, ttl)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        let claim = format!("selected-delivery-{fact_id}");
        self.claims
            .lock()
            .unwrap()
            .push((fact_id.into(), claim.clone()));
        Ok(Some(claim))
    }

    async fn renew_lifecycle_claim(
        &self,
        fact_id: &str,
        claim: &str,
        ttl: Duration,
    ) -> Result<bool, SessionRepositoryError> {
        let renewed = self.renewal_current.load(Ordering::SeqCst)
            && self
                .repository
                .renew_lifecycle_claim(fact_id, claim, ttl)
                .await?;
        self.renewals
            .lock()
            .unwrap()
            .push((fact_id.into(), claim.into(), renewed));
        self.renewed.notify_one();
        Ok(renewed)
    }

    async fn finish_lifecycle_claim(
        &self,
        fact_id: &str,
        claim: &str,
        delivered: bool,
    ) -> Result<(), SessionRepositoryError> {
        if self.finish_current.load(Ordering::SeqCst) {
            self.repository
                .finish_lifecycle_claim(fact_id, claim, delivered)
                .await?;
        }
        self.finish_attempts
            .lock()
            .unwrap()
            .push((fact_id.into(), claim.into(), delivered));
        Ok(())
    }

    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        self.repository.pending_lifecycle().await
    }

    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError> {
        self.repository.complete_lifecycle(fact_id).await
    }
}

#[derive(Default)]
struct GatedDelivery {
    entered: Notify,
    release: Notify,
    delivered: AtomicUsize,
}

#[async_trait::async_trait]
impl LifecycleFactDelivery for GatedDelivery {
    async fn deliver(&self, _fact: &ManagedLifecycleFact) -> Result<(), String> {
        self.entered.notify_one();
        self.release.notified().await;
        self.delivered.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Fixture {
    outbox: Arc<ObservedOutbox>,
    delivery: Arc<GatedDelivery>,
    cancel: CancellationToken,
    run: tokio::task::JoinHandle<Result<(), String>>,
}

impl Fixture {
    async fn entered_delivery() -> Self {
        let outbox = Arc::new(ObservedOutbox {
            repository: SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
            claims: Mutex::default(),
            renewals: Mutex::default(),
            finish_attempts: Mutex::default(),
            renewed: Notify::new(),
            renewal_current: AtomicBool::new(true),
            finish_current: AtomicBool::new(true),
        });
        for id in ["first", "second"] {
            outbox
                .append_lifecycle(ManagedLifecycleFact {
                    id: id.into(),
                    object_id: "session".into(),
                    workspace_id: None,
                    event_type: "session.created".into(),
                    timestamp: 1,
                    runtime_interval: None,
                })
                .await
                .unwrap();
        }
        let delivery = Arc::new(GatedDelivery::default());
        let replay = LifecycleOutboxRelay::new(delivery.clone(), outbox.clone())
            .replay_loop(Duration::from_secs(30))
            .unwrap();
        let cancel = CancellationToken::new();
        let run = tokio::spawn(replay.run(cancel.clone()));
        delivery.entered.notified().await;
        Self {
            outbox,
            delivery,
            cancel,
            run,
        }
    }
}

#[tokio::test]
async fn cancelled_active_delivery_renews_and_finishes_the_same_claim_before_exit() {
    // R1 cancellation after delivery entry retains its live claim heartbeat,
    // completes the selected fact, and stops before claiming the next fact.
    let mut fixture = Fixture::entered_delivery().await;
    let original = fixture.outbox.claims.lock().unwrap()[0].clone();
    assert_eq!(original.0, "first");
    tokio::time::pause();
    fixture.cancel.cancel();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::select! {
        () = fixture.outbox.renewed.notified() => {}
        result = &mut fixture.run => panic!("active delivery must retain its heartbeat: {result:?}"),
    }
    tokio::time::resume();
    let renewals = fixture.outbox.renewals.lock().unwrap().clone();
    assert!(!renewals.is_empty());
    assert!(
        renewals.iter().all(|(fact, claim, renewed)| {
            fact == &original.0 && claim == &original.1 && *renewed
        })
    );
    fixture.delivery.release.notify_one();
    fixture.run.await.unwrap().unwrap();
    assert_eq!(fixture.delivery.delivered.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.outbox.claims.lock().unwrap().len(), 1);
    assert_eq!(
        *fixture.outbox.finish_attempts.lock().unwrap(),
        vec![(original.0, original.1, true)]
    );
    let pending = fixture.outbox.pending_lifecycle().await.unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|fact| fact.id.as_str())
            .collect::<Vec<_>>(),
        vec!["second"]
    );
}

#[tokio::test]
async fn cancelled_active_delivery_honors_renewal_and_finish_fences() {
    // R2 a port-observed stale claim stops at renewal. R3 completion reaches
    // the original finish port with the selected fact/token; its fenced result
    // preserves the pending fact. Each path stops before the next claim.
    for fence_at_renewal in [true, false] {
        let mut fixture = Fixture::entered_delivery().await;
        let original = fixture.outbox.claims.lock().unwrap()[0].clone();
        fixture.cancel.cancel();
        if fence_at_renewal {
            fixture
                .outbox
                .renewal_current
                .store(false, Ordering::SeqCst);
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(10)).await;
            tokio::select! {
                biased;
                () = fixture.outbox.renewed.notified() => {}
                result = &mut fixture.run => panic!("claim loss must be observed at renewal: {result:?}"),
            }
            tokio::time::resume();
            fixture.run.await.unwrap().unwrap();
            assert_eq!(
                *fixture.outbox.renewals.lock().unwrap(),
                vec![(original.0.clone(), original.1.clone(), false)]
            );
            assert!(fixture.outbox.finish_attempts.lock().unwrap().is_empty());
            assert_eq!(fixture.delivery.delivered.load(Ordering::SeqCst), 0);
        } else {
            fixture.outbox.finish_current.store(false, Ordering::SeqCst);
            fixture.delivery.release.notify_one();
            fixture.run.await.unwrap().unwrap();
            assert_eq!(
                *fixture.outbox.finish_attempts.lock().unwrap(),
                vec![(original.0.clone(), original.1.clone(), true)]
            );
            assert_eq!(fixture.delivery.delivered.load(Ordering::SeqCst), 1);
        }
        assert_eq!(fixture.outbox.claims.lock().unwrap().len(), 1);
        let pending = fixture.outbox.pending_lifecycle().await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|fact| fact.id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }
}
