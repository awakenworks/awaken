// @formal surface obligation=webhook.delivery_requires_an_enqueued_fact
// @formal surface obligation=webhook.failed_delivery_remains_pending
// @formal surface obligation=webhook.periodic_retry_never_retires_a_failed_fact
// @formal reason: Managed lifecycle to Webhook dispatch adapter over injected ports.
//! Managed lifecycle delivery over an injected subscription source.
//!
//! The managed webhook bridge connects the managed session lifecycle (the
//! `LifecycleFactDelivery` port, in `awaken-session-application-contract`) to `awaken-webhook`'s
//! delivery machinery. Control injects its subscription/secret source; this
//! bridge owns no authoring store, material, or second outbox.

use std::sync::Arc;

use awaken_session_application_contract::LifecycleFactDelivery;
use awaken_session_state_contract::ManagedLifecycleFact;
use awaken_webhook::{
    ReqwestSender, SubscriptionSource, WebhookDispatcher, WebhookEvent, WebhookSender,
};

pub struct ConfigPlaneLifecycleDelivery {
    dispatcher: Arc<WebhookDispatcher>,
    org_id: Option<String>,
}

impl ConfigPlaneLifecycleDelivery {
    #[must_use]
    pub fn new(dispatcher: Arc<WebhookDispatcher>, org_id: Option<String>) -> Self {
        Self { dispatcher, org_id }
    }
}

#[async_trait::async_trait]
impl LifecycleFactDelivery for ConfigPlaneLifecycleDelivery {
    async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
        let Some(workspace_id) = fact.workspace_id.clone() else {
            return Ok(());
        };
        let event = WebhookEvent::new(
            fact.id.clone(),
            rfc3339(fact.timestamp),
            fact.event_type.clone(),
            fact.object_id.clone(),
            workspace_id,
            self.org_id.clone(),
        );
        let report = self
            .dispatcher
            .dispatch(&event, fact.timestamp)
            .await
            .map_err(|error| error.to_string())?;
        if report.failed.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{} webhook subscription(s) remain pending",
                report.failed.len()
            ))
        }
    }
}

/// Build the delivery adapter over the exact Control-owned subscription source.
/// Coordinator remains the sole owner of the durable lifecycle outbox.
pub fn config_plane_lifecycle_delivery(
    source: Arc<dyn SubscriptionSource>,
    org_id: Option<String>,
) -> Arc<dyn LifecycleFactDelivery> {
    config_plane_lifecycle_delivery_with_sender(source, org_id, Arc::new(ReqwestSender::guarded()))
}

fn config_plane_lifecycle_delivery_with_sender(
    source: Arc<dyn SubscriptionSource>,
    org_id: Option<String>,
    sender: Arc<dyn WebhookSender>,
) -> Arc<dyn LifecycleFactDelivery> {
    Arc::new(ConfigPlaneLifecycleDelivery::new(
        Arc::new(WebhookDispatcher::new(source, sender)),
        org_id,
    ))
}

/// Loopback-only delivery adapter for integration tests. It reuses the exact
/// injected source and event projection; only SSRF transport policy differs.
#[cfg(feature = "test-support")]
pub fn loopback_lifecycle_delivery(
    source: Arc<dyn SubscriptionSource>,
    org_id: Option<String>,
) -> Arc<dyn LifecycleFactDelivery> {
    config_plane_lifecycle_delivery_with_sender(source, org_id, Arc::new(ReqwestSender::default()))
}

/// RFC-3339 UTC from unix seconds, spelled by the wall-clock port's one codec. A
/// negative (pre-epoch) second saturates to the epoch, the port's one pre-epoch
/// rule.
fn rfc3339(secs: i64) -> String {
    awaken_lease_contract::UtcTime::from_duration_since_epoch(std::time::Duration::from_secs(
        u64::try_from(secs).unwrap_or(0),
    ))
    .to_rfc3339_secs()
}

#[cfg(test)]
mod rfc3339_tests {
    use super::rfc3339;

    /// The codec stamps `created_at` on every delivered event, so pin it
    /// against authoritative unix→RFC-3339 UTC vectors covering the boundaries
    /// that break naive implementations: day rollovers, a leap day, a
    /// century-leap year (2000), a non-leap century (2100), and a negative
    /// (pre-epoch) instant, which saturates to the epoch.
    #[test]
    fn matches_known_unix_to_rfc3339_vectors() {
        let cases = [
            (0_i64, "1970-01-01T00:00:00Z"),
            (86_399, "1970-01-01T23:59:59Z"),
            (86_400, "1970-01-02T00:00:00Z"),
            (1_700_000_000, "2023-11-14T22:13:20Z"),
            (1_709_208_000, "2024-02-29T12:00:00Z"), // leap day
            (1_583_020_800, "2020-03-01T00:00:00Z"), // day after the 2020 leap day
            (951_825_600, "2000-02-29T12:00:00Z"),   // 2000 is a leap year (÷400)
            (4_102_444_800, "2100-01-01T00:00:00Z"), // 2100 is NOT a leap year (÷100)
            (-1, "1970-01-01T00:00:00Z"),            // pre-epoch saturates to the epoch
        ];
        for (secs, expected) in cases {
            assert_eq!(rfc3339(secs), expected, "rfc3339({secs})");
        }
    }
}
