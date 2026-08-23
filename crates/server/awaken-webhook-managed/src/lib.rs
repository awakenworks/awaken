//! Coordinator-owned lifecycle outbox projection.
//!
//! Coordinator retains the durable fact until the injected Control delivery
//! port succeeds. Notifications only wake the single supervised replay loop;
//! the Session repository remains the source of truth.

mod control_plane;

pub use control_plane::*;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use awaken_session_contract::{
    LifecycleFactDelivery, LifecycleFactNotifier, ManagedSessionRepository,
};
use awaken_tenancy::WorkspaceScope;

/// Map guard-resolved tenancy onto the protocol-neutral Workspace scope used by
/// Coordinator handlers. Apply this layer inside the guard.
pub async fn stamp_workspace_scope(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(tenancy) = request
        .extensions()
        .get::<awaken_authz_enforce::RequestTenancy>()
        .cloned()
    {
        request
            .extensions_mut()
            .insert(WorkspaceScope(tenancy.workspace_id));
    }
    next.run(request).await
}

/// Notification adapter over Coordinator's transactional Session outbox.
pub struct WebhookOutboxNotifier {
    delivery: Arc<dyn LifecycleFactDelivery>,
    session_outbox: Arc<dyn ManagedSessionRepository>,
    draining: Arc<tokio::sync::Mutex<()>>,
    wake: Arc<tokio::sync::Notify>,
    /// One notifier owns one replay loop. Coordinator composition can reserve
    /// the application slot before starting this loop, so a rejected duplicate
    /// binding never leaves a second outbox consumer running.
    started: OnceLock<()>,
}

const RECONCILIATION_INTERVAL: Duration = Duration::from_secs(30);

impl WebhookOutboxNotifier {
    #[must_use]
    pub fn with_delivery(
        delivery: Arc<dyn LifecycleFactDelivery>,
        outbox: Arc<dyn ManagedSessionRepository>,
        service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    ) -> Self {
        Self::with_delivery_interval(delivery, outbox, RECONCILIATION_INTERVAL, service_lifecycle)
    }

    /// Test seam for the replay interval. Production always uses
    /// [`RECONCILIATION_INTERVAL`].
    #[doc(hidden)]
    #[must_use]
    pub fn with_delivery_interval(
        delivery: Arc<dyn LifecycleFactDelivery>,
        outbox: Arc<dyn ManagedSessionRepository>,
        interval: Duration,
        service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    ) -> Self {
        let sink = Self::deferred(delivery, outbox);
        sink.register_reconciliation(interval, service_lifecycle)
            .expect("new lifecycle outbox notifier starts once");
        sink
    }

    /// Construct the canonical outbox consumer without starting its supervisor.
    /// Coordinator uses this narrow composition seam to reserve the
    /// SessionApplication notifier slot first.
    #[doc(hidden)]
    #[must_use]
    pub fn deferred(
        delivery: Arc<dyn LifecycleFactDelivery>,
        outbox: Arc<dyn ManagedSessionRepository>,
    ) -> Self {
        Self {
            delivery,
            session_outbox: outbox,
            draining: Arc::new(tokio::sync::Mutex::new(())),
            wake: Arc::new(tokio::sync::Notify::new()),
            started: OnceLock::new(),
        }
    }

    /// Start the production reconciliation cadence after the notifier has been
    /// installed in its one SessionApplication owner.
    #[doc(hidden)]
    pub fn start(
        &self,
        service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    ) -> Result<(), &'static str> {
        self.register_reconciliation(RECONCILIATION_INTERVAL, service_lifecycle)
    }

    fn register_reconciliation(
        &self,
        interval: Duration,
        service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    ) -> Result<(), &'static str> {
        self.started
            .set(())
            .map_err(|_| "lifecycle outbox notifier is already started")?;
        let delivery = self.delivery.clone();
        let session_outbox = self.session_outbox.clone();
        let draining = self.draining.clone();
        let wake = self.wake.clone();
        service_lifecycle.spawn("coordinator-webhook-outbox", move |cancel| async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                Self::drain_once(&delivery, &session_outbox, &draining).await;
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = ticker.tick() => {}
                    () = wake.notified() => {}
                }
            }
            Ok(())
        });
        Ok(())
    }

    async fn drain_once(
        delivery: &Arc<dyn LifecycleFactDelivery>,
        session_outbox: &Arc<dyn ManagedSessionRepository>,
        draining: &Arc<tokio::sync::Mutex<()>>,
    ) {
        let _guard = draining.lock().await;
        let rows = match session_outbox.pending_lifecycle().await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "Session lifecycle outbox remains pending");
                return;
            }
        };
        for row in rows {
            if delivery.deliver(&row).await.is_ok()
                && let Err(error) = session_outbox.complete_lifecycle(&row.id).await
            {
                tracing::warn!(fact_id = %row.id, %error, "Session lifecycle receipt remains pending");
            }
        }
    }
}

impl LifecycleFactNotifier for WebhookOutboxNotifier {
    fn notify(&self) {
        self.wake.notify_one();
    }
}
