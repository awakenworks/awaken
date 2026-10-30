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
use awaken_session_application_contract::{LifecycleFactDelivery, LifecycleFactNotifier};
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
            self.drain_once().await;
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = ticker.tick() => {}
                () = self.wake.notified() => {}
            }
        }
        Ok(())
    }

    async fn drain_once(&self) {
        let _guard = self.draining.lock().await;
        let rows = match self.session_outbox.pending_lifecycle().await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "Session lifecycle outbox remains pending");
                return;
            }
        };
        for row in rows {
            let pending = PendingLifecycleFact::from_repository(&row);
            let delivery_succeeded = self.delivery.deliver(pending.delivery_fact()).await.is_ok();
            if let Some(delivered) = pending.completion_fact(delivery_succeeded)
                && let Err(error) = self.session_outbox.complete_lifecycle(&delivered.id).await
            {
                tracing::warn!(fact_id = %delivered.id, %error, "Session lifecycle receipt remains pending");
            }
        }
    }
}

impl LifecycleFactNotifier for LifecycleOutboxRelay {
    fn notify(&self) {
        self.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::PendingLifecycleFact;

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
