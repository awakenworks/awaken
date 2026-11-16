// @formal entrypoint webhook.committed_outbox_row_is_recoverable
// @formal entrypoint webhook.delivery_requires_an_enqueued_fact
// @formal entrypoint webhook.failed_delivery_remains_pending
// @formal entrypoint webhook.outbox_fact_identity_is_stable
// @formal entrypoint webhook.periodic_retry_never_retires_a_failed_fact
//! The one consumer of the durable Session lifecycle outbox.
//!
//! A committed lifecycle fact stays pending until the injected delivery port
//! succeeds. Notifications only wake the single replay loop, a periodic pass
//! retries whatever is still pending, and the Session repository remains the
//! source of truth. The composition root runs the loop under its own supervisor.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use awaken_runtime_contract::CancellationToken;
use awaken_session_application_contract::LifecycleFactNotifier;
use awaken_session_contract::LifecycleFactDelivery;
use awaken_session_contract::LifecycleOutbox;

/// How often the replay loop retries pending facts without a notification.
pub const LIFECYCLE_OUTBOX_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(30);

/// Notification adapter over the transactional Session lifecycle outbox.
pub struct LifecycleOutboxRelay {
    delivery: Arc<dyn LifecycleFactDelivery>,
    session_outbox: Arc<dyn LifecycleOutbox>,
    draining: Arc<tokio::sync::Mutex<()>>,
    wake: Arc<tokio::sync::Notify>,
    /// One relay owns one replay loop. The composition root can install the
    /// relay as the SessionApplication notifier before taking its loop, so a
    /// rejected duplicate binding never leaves a second outbox consumer running.
    started: OnceLock<()>,
}

/// Borrow one fact returned by the durable lifecycle outbox. The same borrow is
/// used for delivery and, only after delivery succeeds, for the completion key;
/// the relay therefore cannot mint a second retry or receipt identity.
struct PendingLifecycleFact<'a, T> {
    fact: &'a T,
}

impl<'a, T> PendingLifecycleFact<'a, T> {
    fn from_repository(fact: &'a T) -> Self {
        Self { fact }
    }

    fn delivery_fact(&self) -> &'a T {
        self.fact
    }

    fn completion_fact(&self, delivery_succeeded: bool) -> Option<&'a T> {
        delivery_succeeded.then_some(self.fact)
    }
}

/// The relay's replay loop, owned by whichever supervisor runs it.
pub struct LifecycleOutboxReplay {
    delivery: Arc<dyn LifecycleFactDelivery>,
    session_outbox: Arc<dyn LifecycleOutbox>,
    draining: Arc<tokio::sync::Mutex<()>>,
    wake: Arc<tokio::sync::Notify>,
    interval: Duration,
}

impl LifecycleOutboxRelay {
    /// Construct the canonical outbox consumer without starting its loop.
    #[must_use]
    pub fn new(delivery: Arc<dyn LifecycleFactDelivery>, outbox: Arc<dyn LifecycleOutbox>) -> Self {
        Self {
            delivery,
            session_outbox: outbox,
            draining: Arc::new(tokio::sync::Mutex::new(())),
            wake: Arc::new(tokio::sync::Notify::new()),
            started: OnceLock::new(),
        }
    }

    /// Take the relay's one replay loop. Production passes
    /// [`LIFECYCLE_OUTBOX_RECONCILIATION_INTERVAL`]; a second call fails, so
    /// one relay never has two consumers.
    pub fn replay_loop(&self, interval: Duration) -> Result<LifecycleOutboxReplay, &'static str> {
        self.started
            .set(())
            .map_err(|_| "lifecycle outbox notifier is already started")?;
        Ok(LifecycleOutboxReplay {
            delivery: self.delivery.clone(),
            session_outbox: self.session_outbox.clone(),
            draining: self.draining.clone(),
            wake: self.wake.clone(),
            interval,
        })
    }
}

impl LifecycleOutboxReplay {
    /// Drain on every notification and every interval tick until cancelled.
    pub async fn run(self, cancel: CancellationToken) -> Result<(), String> {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            self.drain_once(&cancel).await;
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = ticker.tick() => {}
                () = self.wake.notified() => {}
            }
        }
        Ok(())
    }

    async fn drain_once(&self, cancel: &CancellationToken) {
        const CLAIM_TTL: Duration = Duration::from_secs(30);
        const BATCH_SIZE: std::num::NonZeroU32 = std::num::NonZeroU32::new(128).unwrap();

        let _guard = self.draining.lock().await;
        let mut after = None;
        while !cancel.is_cancelled() {
            let batch = match self
                .session_outbox
                .list_pending(after.as_ref(), BATCH_SIZE)
                .await
            {
                Ok(batch) => batch,
                Err(error) => {
                    tracing::warn!(%error, "Session lifecycle outbox remains pending");
                    return;
                }
            };
            let (facts, next) = batch.into_parts();
            for row in facts {
                if cancel.is_cancelled() {
                    return;
                }
                let pending = PendingLifecycleFact::from_repository(&row);
                let fact = pending.delivery_fact();
                let claim = match self
                    .session_outbox
                    .claim_lifecycle(&fact.id, CLAIM_TTL)
                    .await
                {
                    Ok(Some(claim)) => claim,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(fact_id = %fact.id, %error, "Session lifecycle delivery claim failed");
                        continue;
                    }
                };
                // Cancellation stops the drain before the next claim, never
                // inside one: a started delivery runs to its receipt, keeping
                // its claim alive, so shutdown joins it (ServiceLifecycle's
                // deadline bounds that join). Abandoning it would leave the
                // fact claimed and unacknowledged until the claim expired.
                let delivery = self.delivery.deliver(fact);
                tokio::pin!(delivery);
                let mut heartbeat = tokio::time::interval(CLAIM_TTL / 3);
                heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                heartbeat.tick().await;
                // Complete the selected delivery under its live claim. The
                // service lifecycle joins this drain through its shared deadline.
                let result = loop {
                    tokio::select! {
                        result = &mut delivery => break Some(result),
                        _ = heartbeat.tick() => {
                            match self.session_outbox.renew_lifecycle_claim(&fact.id, &claim, CLAIM_TTL).await {
                                Ok(true) => {}
                                Ok(false) => break None,
                                Err(error) => {
                                    tracing::warn!(fact_id = %fact.id, %error, "Session lifecycle delivery claim renewal failed");
                                    break None;
                                }
                            }
                        }
                    }
                };
                let Some(result) = result else {
                    continue;
                };
                if let Err(error) = &result {
                    tracing::warn!(fact_id = %fact.id, %error, "Session lifecycle delivery remains pending");
                }
                let delivered = pending.completion_fact(result.is_ok()).is_some();
                if let Err(error) = self
                    .session_outbox
                    .finish_lifecycle_claim(&fact.id, &claim, delivered)
                    .await
                {
                    tracing::warn!(fact_id = %fact.id, %error, "Session lifecycle receipt remains pending");
                }
            }
            if next.is_none() || next == after {
                return;
            }
            after = next;
        }
    }
}

impl LifecycleFactNotifier for LifecycleOutboxRelay {
    fn notify(&self) {
        self.wake.notify_one();
    }
}

#[cfg(test)]
#[path = "lifecycle_outbox_relay/drain_tests.rs"]
mod drain_tests;

#[cfg(test)]
mod tests {
    use super::PendingLifecycleFact;

    use super::{CancellationToken, LifecycleFactDelivery, LifecycleOutbox, LifecycleOutboxRelay};
    use awaken_session_contract::ManagedLifecycleFact;
    use awaken_session_store::SqliteManagedSessionRepository;
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Default)]
    struct FlakyReceiver(std::sync::atomic::AtomicBool);

    #[async_trait::async_trait]
    impl LifecycleFactDelivery for FlakyReceiver {
        async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
            if fact.id == "128" || self.0.load(std::sync::atomic::Ordering::Relaxed) {
                Ok(())
            } else {
                Err("receiver unavailable".into())
            }
        }
    }

    struct GatedReceiver {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl LifecycleFactDelivery for GatedReceiver {
        async fn deliver(&self, _fact: &ManagedLifecycleFact) -> Result<(), String> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancellation_during_a_delivery_completes_and_acknowledges_it() {
        // Cause/effect: C1 one pending fact; C2 its delivery has started; C3
        // cancellation arrives while it is in flight; C4 the receiver returns.
        // E1 the drain does not return before C4; E2 after C4 the fact is
        // acknowledged; E3 a second pending fact is not claimed after C3.
        let rows = Arc::new(
            SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        for id in ["first", "second"] {
            rows.append_lifecycle(ManagedLifecycleFact {
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
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let receiver = Arc::new(GatedReceiver {
            entered: entered.clone(),
            release: release.clone(),
        });
        let replay = Arc::new(
            LifecycleOutboxRelay::new(receiver, rows.clone())
                .replay_loop(Duration::from_secs(30))
                .unwrap(),
        );
        let cancel = CancellationToken::new();
        let drain = tokio::spawn({
            let replay = replay.clone();
            let cancel = cancel.clone();
            async move { replay.drain_once(&cancel).await }
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("C2 the first delivery started");
        cancel.cancel();
        tokio::task::yield_now().await;
        assert!(
            !drain.is_finished(),
            "E1 cancellation does not abandon the delivery"
        );
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("E1 the drain returns once the receiver does")
            .unwrap();
        let pending: Vec<String> = rows
            .pending_lifecycle()
            .await
            .unwrap()
            .into_iter()
            .map(|fact| fact.id)
            .collect();
        assert_eq!(
            pending,
            ["second"],
            "E2 first acknowledged; E3 second not claimed"
        );
    }

    #[tokio::test]
    async fn failed_batch_does_not_block_later_facts_and_is_retried() {
        let rows = Arc::new(
            SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        for index in 0..=128 {
            rows.append_lifecycle(ManagedLifecycleFact {
                id: format!("{index:03}"),
                object_id: "session".into(),
                workspace_id: None,
                event_type: "session.created".into(),
                timestamp: 1,
                runtime_interval: None,
            })
            .await
            .unwrap();
        }
        let receiver = Arc::new(FlakyReceiver::default());
        let replay = LifecycleOutboxRelay::new(receiver.clone(), rows.clone())
            .replay_loop(Duration::from_secs(30))
            .unwrap();
        let cancel = CancellationToken::new();
        replay.drain_once(&cancel).await;
        let pending = rows.pending_lifecycle().await.unwrap();
        assert_eq!(pending.len(), 128);
        assert!(!pending.iter().any(|fact| fact.id == "128"));
        receiver.0.store(true, std::sync::atomic::Ordering::Relaxed);
        replay.drain_once(&cancel).await;
        assert!(rows.pending_lifecycle().await.unwrap().is_empty());
    }

    #[test]
    fn pending_fact_completion_uses_the_delivered_identity_only_after_success() {
        // Cause/effect graph: C1 one repository-returned pending identity enters
        // the notifier; C2 delivery succeeds; C3 delivery fails; C4 the failed
        // fact is retried. E1 every attempt borrows the exact pending identity;
        // E2 success selects that same identity for completion; E3 failure and
        // repeated failure select no completion. Decision table:
        // R1=C1+C2 -> E1+E2; R2=C1+C3 -> E1+E3;
        // R3=C1+C3+C4 -> E1+E3 on both attempts.
        // Mutation targets: making failure complete or success retain must break
        // R2/R3 or R1 respectively; replacing the borrowed identity breaks E1/E2.
        let identity = 41_u64;
        let pending = PendingLifecycleFact::from_repository(&identity);

        assert!(std::ptr::eq(pending.delivery_fact(), &identity), "R1-R3/E1");
        let completed = pending
            .completion_fact(true)
            .expect("R1 success selects completion");
        assert!(std::ptr::eq(completed, &identity), "R1/E2");
        assert!(pending.completion_fact(false).is_none(), "R2/E3");
        assert!(pending.completion_fact(false).is_none(), "R3/E3");
    }
}

#[cfg(kani)]
/// @obligation webhook.delivery_requires_an_enqueued_fact
/// @obligation webhook.failed_delivery_remains_pending
/// @obligation webhook.outbox_fact_identity_is_stable
/// @obligation webhook.periodic_retry_never_retires_a_failed_fact
#[kani::proof]
fn pending_lifecycle_fact_completes_iff_delivery_succeeds_with_the_exact_identity() {
    // The production admission/settlement owner is exhausted over both delivery
    // outcomes and an arbitrary identity. No assumptions, stubs or domain
    // restrictions are required.
    let identity: u64 = kani::any();
    let delivery_succeeded: bool = kani::any();
    let pending = PendingLifecycleFact::from_repository(&identity);

    assert!(std::ptr::eq(pending.delivery_fact(), &identity));
    let completion = pending.completion_fact(delivery_succeeded);
    assert_eq!(completion.is_some(), delivery_succeeded);
    if let Some(completed) = completion {
        assert!(std::ptr::eq(completed, &identity));
        assert_eq!(*completed, identity);
    } else {
        assert!(!delivery_succeeded);
        assert!(pending.completion_fact(false).is_none());
    }
}
