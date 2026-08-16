//! Delivery: project a committed [`WebhookEvent`] to every matching subscription,
//! signed (Standard Webhooks) and retried, auto-disabling an endpoint after too
//! many consecutive failures. The HTTP transport is a port so a real reqwest
//! client backs production while a scripted sender drives unit tests; the e2e
//! uses the reqwest impl against a real receiver.

use std::sync::Arc;
#[cfg(any(test, feature = "test-support"))]
use std::time::Duration;

use async_trait::async_trait;
use awaken_outbound_http::{GuardedHttpSender, HttpSender, status_is_retryable};

use crate::event::WebhookEvent;
use crate::signing::signature_header;

/// A subscription with its signing secret already resolved — the shape the
/// dispatcher signs and delivers. Persistence and secret-sealing live entirely
/// behind [`SubscriptionSource`] (the config plane + `SecretStore`), so the
/// webhook domain never touches a store or a `SecretRef`.
#[derive(Debug, Clone)]
pub struct ResolvedSubscription {
    pub id: String,
    pub url: String,
    /// The `whsec_` signing secret, resolved from the vault by the source.
    pub secret: String,
}

/// Durable state after one failed event delivery is recorded by the subscription
/// authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionFailureState {
    /// The endpoint remains live and this event is still retryable.
    Active,
    /// The failure threshold atomically disabled the endpoint.
    Disabled,
    /// An operator concurrently deleted the endpoint; no obligation remains.
    Removed,
}

/// A subscription-authority failure. Dispatch never converts this into an empty
/// subscription set because doing so would retire the durable event without proof
/// that its delivery obligations were observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchError {
    pub operation: &'static str,
    pub subscription_id: Option<String>,
    pub message: String,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(id) = &self.subscription_id {
            write!(
                f,
                "webhook subscription {id} {} failed: {}",
                self.operation, self.message
            )
        } else {
            write!(
                f,
                "webhook subscription {} failed: {}",
                self.operation, self.message
            )
        }
    }
}

impl std::error::Error for DispatchError {}

/// The port the dispatcher drives: the live (non-disabled) matching subscriptions
/// for a `(workspace, event_type)` with secrets already resolved, plus the
/// auto-disable write. The assembly backs it with the config-plane `WebhookStore`
/// + `SecretStore`; a test backs it in memory.
#[async_trait]
pub trait SubscriptionSource: Send + Sync {
    async fn matching(
        &self,
        workspace_id: &str,
        event_type: &str,
    ) -> Result<Vec<ResolvedSubscription>, String>;
    /// Durably reset the consecutive-failure count after a 2xx acknowledgement.
    async fn record_success(&self, id: &str) -> Result<(), String>;
    /// Durably record one failed event delivery and atomically auto-disable at the
    /// threshold. The returned state tells the dispatcher whether an obligation
    /// remains.
    async fn record_failure(
        &self,
        id: &str,
        failure_threshold: u32,
    ) -> Result<SubscriptionFailureState, String>;
}

/// The HTTP transport for a single delivery attempt. Returns the response status
/// code, or a transport error string.
#[async_trait]
pub trait WebhookSender: Send + Sync {
    async fn post(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
        body: String,
    ) -> Result<u16, String>;
}

/// Compatibility wrapper over the one lower-layer guarded HTTP implementation.
/// Webhook owns signing and retry semantics, not a second network policy.
pub struct ReqwestSender {
    inner: GuardedHttpSender,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for ReqwestSender {
    fn default() -> Self {
        Self {
            inner: GuardedHttpSender::with_timeout(Duration::from_secs(10)),
        }
    }
}

impl ReqwestSender {
    #[must_use]
    pub fn guarded() -> Self {
        Self {
            inner: GuardedHttpSender::guarded(),
        }
    }

    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            inner: GuardedHttpSender::with_timeout(timeout),
        }
    }
}

#[async_trait]
impl WebhookSender for ReqwestSender {
    async fn post(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
        body: String,
    ) -> Result<u16, String> {
        self.inner.post(url, headers, body).await
    }
}

/// The outcome of dispatching one event across its matching subscriptions.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DispatchReport {
    /// Subscription ids that accepted the delivery (2xx).
    pub delivered: Vec<String>,
    /// Subscription ids whose retryable delivery failed every attempt and remain
    /// active, so the durable event must stay pending.
    pub failed: Vec<String>,
    /// Subscription ids that permanently rejected this event (for example 404 or
    /// 422). The event is terminal for this subscription, but the durable failure
    /// count still contributes to eventual auto-disable across later events.
    pub rejected: Vec<String>,
    /// Subscription ids auto-disabled this dispatch (crossed the failure threshold).
    pub disabled: Vec<String>,
}

/// Projects committed events to subscriptions. Consecutive-failure counts live in
/// the subscription authority; crossing `failure_threshold` auto-disables the
/// endpoint durably (and a success resets the count).
pub struct WebhookDispatcher {
    source: Arc<dyn SubscriptionSource>,
    sender: Arc<dyn WebhookSender>,
    max_attempts: u32,
    failure_threshold: u32,
}

impl WebhookDispatcher {
    /// A dispatcher with production defaults (3 attempts/delivery, auto-disable at
    /// 20 consecutive failures — matching CMA's ~20-failure auto-disable).
    pub fn new(source: Arc<dyn SubscriptionSource>, sender: Arc<dyn WebhookSender>) -> Self {
        Self {
            source,
            sender,
            max_attempts: 3,
            failure_threshold: 20,
        }
    }

    /// Override the retry / auto-disable thresholds (tests).
    pub fn with_thresholds(mut self, max_attempts: u32, failure_threshold: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self.failure_threshold = failure_threshold.max(1);
        self
    }

    /// Deliver `event` to every live matching subscription. `timestamp` is the
    /// unix-seconds signing/`webhook-timestamp` value (passed in, not read from a
    /// clock, so delivery is reproducible and the caller owns time).
    pub async fn dispatch(
        &self,
        event: &WebhookEvent,
        timestamp: i64,
    ) -> Result<DispatchReport, DispatchError> {
        event.validate().map_err(|message| DispatchError {
            operation: "event validation",
            subscription_id: None,
            message: message.to_string(),
        })?;
        let body = event.to_body();
        let subs = self
            .source
            .matching(&event.data.workspace_id, &event.data.event_type)
            .await
            .map_err(|message| DispatchError {
                operation: "enumeration",
                subscription_id: None,
                message,
            })?;

        let mut report = DispatchReport::default();
        for sub in subs {
            let Ok(signature) = signature_header(&sub.secret, &event.id, timestamp, &body) else {
                // A malformed secret can never verify. Record it through the same
                // durable failure transition as an HTTP rejection; do not leave a
                // poison event pending forever or bypass auto-disable accounting.
                let state = self.record_failure(&sub.id).await?;
                report.rejected.push(sub.id.clone());
                if state == SubscriptionFailureState::Disabled {
                    report.disabled.push(sub.id.clone());
                }
                continue;
            };
            let headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("webhook-id".to_string(), event.id.clone()),
                ("webhook-timestamp".to_string(), timestamp.to_string()),
                ("webhook-signature".to_string(), signature),
            ];

            let mut outcome = AttemptOutcome::RetryableFailure;
            for _ in 0..self.max_attempts {
                match self
                    .sender
                    .post(&sub.url, headers.clone(), body.clone())
                    .await
                {
                    Ok(code) if (200..300).contains(&code) => {
                        outcome = AttemptOutcome::Delivered;
                        break;
                    }
                    Ok(code) if !status_is_retryable(code) => {
                        outcome = AttemptOutcome::PermanentRejection;
                        break;
                    }
                    Ok(_) | Err(_) => continue,
                }
            }

            match outcome {
                AttemptOutcome::Delivered => {
                    self.source
                        .record_success(&sub.id)
                        .await
                        .map_err(|message| DispatchError {
                            operation: "success recording",
                            subscription_id: Some(sub.id.clone()),
                            message,
                        })?;
                    report.delivered.push(sub.id);
                }
                AttemptOutcome::PermanentRejection => {
                    let state = self.record_failure(&sub.id).await?;
                    report.rejected.push(sub.id.clone());
                    if state == SubscriptionFailureState::Disabled {
                        report.disabled.push(sub.id);
                    }
                }
                AttemptOutcome::RetryableFailure => match self.record_failure(&sub.id).await? {
                    SubscriptionFailureState::Active => report.failed.push(sub.id),
                    SubscriptionFailureState::Disabled => report.disabled.push(sub.id),
                    SubscriptionFailureState::Removed => {}
                },
            }
        }
        Ok(report)
    }

    async fn record_failure(
        &self,
        subscription_id: &str,
    ) -> Result<SubscriptionFailureState, DispatchError> {
        self.source
            .record_failure(subscription_id, self.failure_threshold)
            .await
            .map_err(|message| DispatchError {
                operation: "failure recording",
                subscription_id: Some(subscription_id.to_string()),
                message,
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptOutcome {
    Delivered,
    RetryableFailure,
    PermanentRejection,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"; // awaken-allow: secret (test sample key)

    /// An in-memory [`SubscriptionSource`]: holds one resolved subscription and a
    /// disabled set, so `matching` fences out anything the dispatcher auto-disabled.
    #[derive(Default)]
    struct TestSource {
        subs: Vec<ResolvedSubscription>,
        disabled: Mutex<HashSet<String>>,
        failures: Mutex<HashMap<String, u32>>,
    }
    impl TestSource {
        fn with(sub: ResolvedSubscription) -> Arc<Self> {
            Arc::new(Self {
                subs: vec![sub],
                disabled: Mutex::new(HashSet::new()),
                failures: Mutex::new(HashMap::new()),
            })
        }
        fn is_disabled(&self, id: &str) -> bool {
            self.disabled.lock().unwrap().contains(id)
        }
    }
    #[async_trait]
    impl SubscriptionSource for TestSource {
        async fn matching(
            &self,
            _ws: &str,
            _event: &str,
        ) -> Result<Vec<ResolvedSubscription>, String> {
            let disabled = self.disabled.lock().unwrap();
            Ok(self
                .subs
                .iter()
                .filter(|s| !disabled.contains(&s.id))
                .cloned()
                .collect())
        }
        async fn record_success(&self, id: &str) -> Result<(), String> {
            self.failures.lock().unwrap().remove(id);
            Ok(())
        }
        async fn record_failure(
            &self,
            id: &str,
            failure_threshold: u32,
        ) -> Result<SubscriptionFailureState, String> {
            let count = {
                let mut failures = self.failures.lock().unwrap();
                let count = failures.entry(id.to_string()).or_insert(0);
                *count += 1;
                *count
            };
            if count >= failure_threshold {
                self.disabled.lock().unwrap().insert(id.to_string());
                Ok(SubscriptionFailureState::Disabled)
            } else {
                Ok(SubscriptionFailureState::Active)
            }
        }
    }

    /// A sender scripted to fail the first `fail_n` calls, then succeed.
    struct ScriptedSender {
        calls: Mutex<u32>,
        fail_first: u32,
        #[allow(clippy::type_complexity)]
        captured: Mutex<Vec<(String, Vec<(String, String)>, String)>>,
    }
    impl ScriptedSender {
        fn new(fail_first: u32) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(0),
                fail_first,
                captured: Mutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl WebhookSender for ScriptedSender {
        async fn post(
            &self,
            url: &str,
            headers: Vec<(String, String)>,
            body: String,
        ) -> Result<u16, String> {
            let n = {
                let mut c = self.calls.lock().unwrap();
                *c += 1;
                *c
            };
            self.captured
                .lock()
                .unwrap()
                .push((url.to_string(), headers, body));
            if n <= self.fail_first {
                Err("boom".into())
            } else {
                Ok(200)
            }
        }
    }

    fn resolved(id: &str) -> ResolvedSubscription {
        ResolvedSubscription {
            id: id.to_string(),
            url: "https://example/hook".to_string(),
            secret: SECRET.to_string(),
        }
    }

    fn event() -> WebhookEvent {
        WebhookEvent::new(
            "event_1",
            "2026-07-09T00:00:00Z",
            "session.status_idled",
            "sesn_1",
            "wrkspc_a",
            None,
        )
    }

    #[tokio::test]
    async fn delivers_signed_with_standard_headers_and_retries() {
        let source = TestSource::with(resolved("wh_1"));
        let sender = ScriptedSender::new(1); // fail once, then succeed on retry
        let dispatcher =
            WebhookDispatcher::new(source.clone(), sender.clone()).with_thresholds(3, 20);

        let report = dispatcher.dispatch(&event(), 1_700_000_000).await.unwrap();
        assert_eq!(report.delivered, vec!["wh_1".to_string()]);
        assert!(report.failed.is_empty());

        let calls = sender.captured.lock().unwrap();
        assert_eq!(calls.len(), 2, "one retry after the first failure");
        // The delivered payload is signed and carries the three SW headers.
        let (_url, headers, body) = &calls[1];
        let hdr = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(hdr("webhook-id").as_deref(), Some("event_1"));
        assert_eq!(hdr("webhook-timestamp").as_deref(), Some("1700000000"));
        let sig = hdr("webhook-signature").expect("signature header");
        assert!(
            crate::signing::verify(SECRET, "event_1", 1_700_000_000, body, &sig).unwrap(),
            "delivered signature verifies against the payload"
        );
    }

    #[tokio::test]
    async fn malformed_thread_event_is_rejected_before_subscription_lookup() {
        let source = TestSource::with(resolved("wh_1"));
        let sender = ScriptedSender::new(0);
        let dispatcher = WebhookDispatcher::new(source, sender.clone());
        let malformed = WebhookEvent::new(
            "event_bad_thread",
            "2026-07-09T00:00:00Z",
            "session.thread_idled",
            "sesn_1",
            "wrkspc_a",
            None,
        );

        let error = dispatcher
            .dispatch(&malformed, 1_700_000_000)
            .await
            .unwrap_err();
        assert_eq!(error.operation, "event validation");
        assert_eq!(*sender.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn auto_disables_after_the_failure_threshold() {
        let source = TestSource::with(resolved("wh_1"));
        let sender = ScriptedSender::new(u32::MAX); // always fails
        let dispatcher = WebhookDispatcher::new(source.clone(), sender).with_thresholds(1, 2);

        // First dispatch: 1 consecutive failure — not yet disabled.
        let r1 = dispatcher.dispatch(&event(), 1).await.unwrap();
        assert_eq!(r1.failed, vec!["wh_1".to_string()]);
        assert!(r1.disabled.is_empty());
        assert!(!source.is_disabled("wh_1"));

        // Second dispatch: crosses the threshold → auto-disabled and fenced out.
        let r2 = dispatcher.dispatch(&event(), 2).await.unwrap();
        assert_eq!(r2.disabled, vec!["wh_1".to_string()]);
        assert!(source.is_disabled("wh_1"));

        // Now disabled → no longer selected.
        let r3 = dispatcher.dispatch(&event(), 3).await.unwrap();
        assert!(r3.delivered.is_empty() && r3.failed.is_empty() && r3.disabled.is_empty());
    }

    /// A sender that always returns a fixed HTTP status, counting its calls — for
    /// the non-2xx arm the scripted (Err-or-200) sender cannot express.
    struct CodeSender {
        code: u16,
        calls: Mutex<u32>,
    }
    impl CodeSender {
        fn new(code: u16) -> Arc<Self> {
            Arc::new(Self {
                code,
                calls: Mutex::new(0),
            })
        }
        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }
    #[async_trait]
    impl WebhookSender for CodeSender {
        async fn post(
            &self,
            _url: &str,
            _headers: Vec<(String, String)>,
            _body: String,
        ) -> Result<u16, String> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.code)
        }
    }

    fn resolved_with_secret(id: &str, secret: &str) -> ResolvedSubscription {
        ResolvedSubscription {
            id: id.to_string(),
            url: "https://example/hook".to_string(),
            secret: secret.to_string(),
        }
    }

    #[tokio::test]
    async fn a_non_2xx_response_is_retried_then_failed() {
        let source = TestSource::with(resolved("wh_1"));
        let sender = CodeSender::new(500);
        let dispatcher = WebhookDispatcher::new(source, sender.clone()).with_thresholds(3, 20);

        let report = dispatcher.dispatch(&event(), 1).await.unwrap();
        assert_eq!(report.failed, vec!["wh_1".to_string()]);
        assert!(report.delivered.is_empty());
        assert_eq!(
            sender.calls(),
            3,
            "a non-2xx status is retried up to max_attempts, then fails"
        );
    }

    #[tokio::test]
    async fn a_malformed_secret_fails_without_reaching_the_wire() {
        let source = TestSource::with(resolved_with_secret("wh_1", "whsec_!!!not-base64!!!"));
        let sender = CodeSender::new(200);
        let dispatcher = WebhookDispatcher::new(source, sender.clone()).with_thresholds(3, 20);

        let report = dispatcher.dispatch(&event(), 1).await.unwrap();
        assert_eq!(report.rejected, vec!["wh_1".to_string()]);
        assert_eq!(
            sender.calls(),
            0,
            "a secret that can never sign is a hard failure — no POST is attempted"
        );
    }

    #[tokio::test]
    async fn every_attempt_failing_exhausts_the_retries_then_fails() {
        let source = TestSource::with(resolved("wh_1"));
        let sender = ScriptedSender::new(u32::MAX); // every attempt errors
        let dispatcher = WebhookDispatcher::new(source, sender.clone()).with_thresholds(3, 20);

        let report = dispatcher.dispatch(&event(), 1).await.unwrap();
        assert_eq!(report.failed, vec!["wh_1".to_string()]);
        assert_eq!(
            sender.captured.lock().unwrap().len(),
            3,
            "all three attempts run before the delivery is given up"
        );
    }

    /// A sender driven by a scripted sequence of outcomes (one popped per POST),
    /// so a test can interleave failures and successes across dispatch calls.
    struct SeqSender {
        outcomes: Mutex<std::collections::VecDeque<Result<u16, String>>>,
    }
    impl SeqSender {
        fn new(seq: Vec<Result<u16, String>>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: Mutex::new(seq.into()),
            })
        }
    }
    #[async_trait]
    impl WebhookSender for SeqSender {
        async fn post(
            &self,
            _url: &str,
            _headers: Vec<(String, String)>,
            _body: String,
        ) -> Result<u16, String> {
            self.outcomes.lock().unwrap().pop_front().unwrap_or(Ok(200))
        }
    }

    /// A sender that fails whenever the target url contains `fail_substr`, so a
    /// multi-subscription fan-out can succeed for one endpoint and fail another.
    struct UrlFailSender {
        fail_substr: String,
    }
    #[async_trait]
    impl WebhookSender for UrlFailSender {
        async fn post(
            &self,
            url: &str,
            _headers: Vec<(String, String)>,
            _body: String,
        ) -> Result<u16, String> {
            if url.contains(&self.fail_substr) {
                Err("unreachable endpoint".into())
            } else {
                Ok(200)
            }
        }
    }

    /// A mid-stream success must RESET the consecutive-failure counter, so a later
    /// isolated failure cannot inherit stale counts and trip the auto-disable. With
    /// threshold 2 and 1 attempt: fail, then succeed (reset), then fail again must
    /// leave the endpoint enabled (count back to 1, not 2).
    #[tokio::test]
    async fn a_success_resets_the_failure_counter() {
        let source = TestSource::with(resolved("wh_1"));
        let sender = SeqSender::new(vec![Err("boom".into()), Ok(200), Err("boom".into())]);
        let dispatcher = WebhookDispatcher::new(source.clone(), sender).with_thresholds(1, 2);

        assert_eq!(
            dispatcher.dispatch(&event(), 1).await.unwrap().failed,
            vec!["wh_1".to_string()]
        );
        assert!(
            dispatcher.dispatch(&event(), 2).await.unwrap().delivered == vec!["wh_1".to_string()]
        );
        let r3 = dispatcher.dispatch(&event(), 3).await.unwrap();
        assert_eq!(r3.failed, vec!["wh_1".to_string()]);
        assert!(
            r3.disabled.is_empty() && !source.is_disabled("wh_1"),
            "the intervening success reset the count, so one later failure does not disable"
        );
    }

    /// Fan-out is partitioned and isolated: with two matching subscriptions where one
    /// endpoint is unreachable, the healthy one is still delivered and the report
    /// splits delivered/failed correctly — one subscription's failure never suppresses
    /// another's delivery.
    #[tokio::test]
    async fn fan_out_partitions_and_isolates_subscriptions() {
        let source = Arc::new(TestSource {
            subs: vec![
                ResolvedSubscription {
                    id: "ok".into(),
                    url: "https://good/hook".into(),
                    secret: SECRET.into(),
                },
                ResolvedSubscription {
                    id: "bad".into(),
                    url: "https://bad/hook".into(),
                    secret: SECRET.into(),
                },
            ],
            disabled: Mutex::new(HashSet::new()),
            failures: Mutex::new(HashMap::new()),
        });
        let sender = Arc::new(UrlFailSender {
            fail_substr: "bad".into(),
        });
        let dispatcher = WebhookDispatcher::new(source, sender).with_thresholds(1, 20);

        let report = dispatcher.dispatch(&event(), 1).await.unwrap();
        assert_eq!(report.delivered, vec!["ok".to_string()]);
        assert_eq!(report.failed, vec!["bad".to_string()]);
    }

    /// A `204 No Content` is a valid webhook ack (a very common receiver response):
    /// it is accepted on the first attempt, not retried as a failure.
    #[tokio::test]
    async fn a_204_no_content_is_accepted() {
        let source = TestSource::with(resolved("wh_1"));
        let sender = CodeSender::new(204);
        let dispatcher = WebhookDispatcher::new(source, sender.clone()).with_thresholds(3, 20);

        let report = dispatcher.dispatch(&event(), 1).await.unwrap();
        assert_eq!(report.delivered, vec!["wh_1".to_string()]);
        assert_eq!(sender.calls(), 1, "a 2xx accepts on the first attempt");
    }

    /// No matching subscriptions is a clean empty report — no panic, no POST.
    #[tokio::test]
    async fn no_matching_subscriptions_is_an_empty_report() {
        let source = Arc::new(TestSource::default());
        let sender = CodeSender::new(200);
        let dispatcher = WebhookDispatcher::new(source, sender.clone()).with_thresholds(3, 20);

        let report = dispatcher.dispatch(&event(), 1).await.unwrap();
        assert_eq!(report, DispatchReport::default());
        assert_eq!(sender.calls(), 0, "nothing to deliver → nothing sent");
    }

    struct FailingAuthority {
        enumerate_error: bool,
        state_error: bool,
    }

    #[async_trait]
    impl SubscriptionSource for FailingAuthority {
        async fn matching(
            &self,
            _ws: &str,
            _event: &str,
        ) -> Result<Vec<ResolvedSubscription>, String> {
            if self.enumerate_error {
                Err("repository unavailable".into())
            } else {
                Ok(vec![resolved("wh_1")])
            }
        }
        async fn record_success(&self, _id: &str) -> Result<(), String> {
            if self.state_error {
                Err("state write unavailable".into())
            } else {
                Ok(())
            }
        }
        async fn record_failure(
            &self,
            _id: &str,
            _failure_threshold: u32,
        ) -> Result<SubscriptionFailureState, String> {
            if self.state_error {
                Err("state write unavailable".into())
            } else {
                Ok(SubscriptionFailureState::Active)
            }
        }
    }

    #[tokio::test]
    async fn authority_failures_never_look_like_completed_delivery() {
        // Cause/effect graph: C1 enumeration read fails; C2 POST succeeds but
        // success-state write fails; C3 POST fails and failure-state write fails.
        // Effects: E1 dispatch Err/no POST; E2 dispatch Err after POST; E3 dispatch
        // Err after retries. Decision table R1=C1→E1, R2=C2→E2, R3=C3→E3. In all
        // rules the lifecycle caller retains its durable fact; no empty report can
        // falsely acknowledge an unavailable authority.
        let sender = CodeSender::new(200);
        let d = WebhookDispatcher::new(
            Arc::new(FailingAuthority {
                enumerate_error: true,
                state_error: false,
            }),
            sender.clone(),
        );
        assert_eq!(
            d.dispatch(&event(), 1).await.unwrap_err().operation,
            "enumeration",
            "R1"
        );
        assert_eq!(sender.calls(), 0, "R1");

        let d = WebhookDispatcher::new(
            Arc::new(FailingAuthority {
                enumerate_error: false,
                state_error: true,
            }),
            sender.clone(),
        );
        assert_eq!(
            d.dispatch(&event(), 1).await.unwrap_err().operation,
            "success recording",
            "R2"
        );

        let failing_sender = CodeSender::new(500);
        let d = WebhookDispatcher::new(
            Arc::new(FailingAuthority {
                enumerate_error: false,
                state_error: true,
            }),
            failing_sender,
        );
        assert_eq!(
            d.dispatch(&event(), 1).await.unwrap_err().operation,
            "failure recording",
            "R3"
        );
    }

    /// Every retry attempt carries the SAME `webhook-id` — the at-least-once dedupe
    /// key a receiver uses to drop duplicate retries. If it ever varied per attempt,
    /// a flaky-but-eventually-2xx delivery would be processed more than once.
    #[tokio::test]
    async fn every_retry_attempt_carries_the_same_webhook_id() {
        let source = TestSource::with(resolved("wh_1"));
        let sender = ScriptedSender::new(2); // fail twice, succeed on the third
        let dispatcher = WebhookDispatcher::new(source, sender.clone()).with_thresholds(3, 20);

        dispatcher.dispatch(&event(), 1).await.unwrap();
        let calls = sender.captured.lock().unwrap();
        assert_eq!(calls.len(), 3, "two retries after the first failure");
        let webhook_id = |headers: &[(String, String)]| {
            headers
                .iter()
                .find(|(n, _)| n == "webhook-id")
                .map(|(_, v)| v.clone())
        };
        assert!(
            calls
                .iter()
                .all(|(_, h, _)| webhook_id(h).as_deref() == Some("event_1")),
            "the dedupe key is stable across all retry attempts"
        );
    }

    /// The retry classification's 2xx upper boundary: `(200..300)` is exclusive at
    /// 300, so `299` is still a success accepted on the first attempt, while `300`
    /// (a redirect status — reqwest follows none, so it surfaces as-is) is a
    /// permanent rejection and is never followed or retried.
    #[tokio::test]
    async fn the_2xx_success_band_is_closed_at_300() {
        let source = TestSource::with(resolved("wh_1"));
        let ok = CodeSender::new(299);
        let d = WebhookDispatcher::new(source, ok.clone()).with_thresholds(3, 20);
        let r = d.dispatch(&event(), 1).await.unwrap();
        assert_eq!(r.delivered, vec!["wh_1".to_string()]);
        assert_eq!(ok.calls(), 1, "299 is a 2xx ack on the first attempt");

        let source = TestSource::with(resolved("wh_1"));
        let redirect = CodeSender::new(300);
        let d = WebhookDispatcher::new(source, redirect.clone()).with_thresholds(3, 20);
        let r = d.dispatch(&event(), 1).await.unwrap();
        assert_eq!(r.rejected, vec!["wh_1".to_string()]);
        assert!(r.delivered.is_empty());
        assert_eq!(
            redirect.calls(),
            1,
            "300 is not a success and redirects are terminal"
        );
    }

    /// Cause/effect graph: C1=2xx, C2=transport error, C3=408/425/429/5xx,
    /// C4=redirect/other 4xx; effects E1=delivered+reset, E2=bounded retry and
    /// pending failure, E3=one-attempt terminal rejection. Constraint: exactly one
    /// status class applies. Decision table: R1 C1→E1 (204/299 tests); R2 C2→E2
    /// (`every_attempt_failing...`); R3 C3→E2 (408/425/429/500/599 below); R4
    /// C4→E3 (300/400/404/410/422 below). This test owns R3/R4 boundaries.
    #[tokio::test]
    async fn retryable_statuses_exhaust_but_permanent_statuses_stop_after_one_attempt() {
        for code in [408u16, 425, 429, 500, 599] {
            let source = TestSource::with(resolved("wh_1"));
            let sender = CodeSender::new(code);
            let d = WebhookDispatcher::new(source, sender.clone()).with_thresholds(3, 20);
            let r = d.dispatch(&event(), 1).await.unwrap();
            assert_eq!(r.failed, vec!["wh_1".to_string()], "R3: {code}");
            assert!(r.rejected.is_empty(), "R3: {code}");
            assert_eq!(sender.calls(), 3, "R3: {code} exhausts max_attempts");
        }
        for code in [300u16, 400, 404, 410, 422] {
            let source = TestSource::with(resolved("wh_1"));
            let sender = CodeSender::new(code);
            let d = WebhookDispatcher::new(source, sender.clone()).with_thresholds(3, 20);
            let r = d.dispatch(&event(), 1).await.unwrap();
            assert_eq!(r.rejected, vec!["wh_1".to_string()], "R4: {code}");
            assert!(r.failed.is_empty(), "R4: {code}");
            assert_eq!(sender.calls(), 1, "R4: {code} is not retried");
        }
    }
}
