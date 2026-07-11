//! Delivery: project a committed [`WebhookEvent`] to every matching subscription,
//! signed (Standard Webhooks) and retried, auto-disabling an endpoint after too
//! many consecutive failures. The HTTP transport is a port so a real reqwest
//! client backs production while a scripted sender drives unit tests; the e2e
//! uses the reqwest impl against a real receiver.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

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

/// The port the dispatcher drives: the live (non-disabled) matching subscriptions
/// for a `(workspace, event_type)` with secrets already resolved, plus the
/// auto-disable write. The assembly backs it with the config-plane `WebhookStore`
/// + `SecretStore`; a test backs it in memory.
#[async_trait]
pub trait SubscriptionSource: Send + Sync {
    async fn matching(&self, workspace_id: &str, event_type: &str) -> Vec<ResolvedSubscription>;
    /// Suspend delivery to `id` (auto-disable after repeated failures).
    async fn disable(&self, id: &str);
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

/// A reqwest-backed sender (production).
pub struct ReqwestSender {
    client: reqwest::Client,
}

impl Default for ReqwestSender {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none()) // never follow redirects
                .build()
                .expect("reqwest client builds"),
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
        let mut req = self.client.post(url).body(body);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        Ok(resp.status().as_u16())
    }
}

/// The outcome of dispatching one event across its matching subscriptions.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DispatchReport {
    /// Subscription ids that accepted the delivery (2xx).
    pub delivered: Vec<String>,
    /// Subscription ids whose delivery failed every attempt this dispatch.
    pub failed: Vec<String>,
    /// Subscription ids auto-disabled this dispatch (crossed the failure threshold).
    pub disabled: Vec<String>,
}

/// Projects committed events to subscriptions. Consecutive-failure counts are held
/// per subscription; crossing `failure_threshold` auto-disables the endpoint (and
/// a success resets the count).
pub struct WebhookDispatcher {
    source: Arc<dyn SubscriptionSource>,
    sender: Arc<dyn WebhookSender>,
    max_attempts: u32,
    failure_threshold: u32,
    failures: Mutex<HashMap<String, u32>>,
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
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Override the retry / auto-disable thresholds (tests).
    pub fn with_thresholds(mut self, max_attempts: u32, failure_threshold: u32) -> Self {
        self.max_attempts = max_attempts;
        self.failure_threshold = failure_threshold;
        self
    }

    /// Deliver `event` to every live matching subscription. `timestamp` is the
    /// unix-seconds signing/`webhook-timestamp` value (passed in, not read from a
    /// clock, so delivery is reproducible and the caller owns time).
    pub async fn dispatch(&self, event: &WebhookEvent, timestamp: i64) -> DispatchReport {
        let body = event.to_body();
        let subs = self
            .source
            .matching(&event.data.workspace_id, &event.data.event_type)
            .await;

        let mut report = DispatchReport::default();
        for sub in subs {
            let Ok(signature) = signature_header(&sub.secret, &event.id, timestamp, &body) else {
                // A malformed secret can never verify — treat as a hard failure.
                report.failed.push(sub.id.clone());
                continue;
            };
            let headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("webhook-id".to_string(), event.id.clone()),
                ("webhook-timestamp".to_string(), timestamp.to_string()),
                ("webhook-signature".to_string(), signature),
            ];

            let mut ok = false;
            for _ in 0..self.max_attempts {
                match self
                    .sender
                    .post(&sub.url, headers.clone(), body.clone())
                    .await
                {
                    Ok(code) if (200..300).contains(&code) => {
                        ok = true;
                        break;
                    }
                    _ => continue,
                }
            }

            if ok {
                self.failures.lock().unwrap().remove(&sub.id);
                report.delivered.push(sub.id);
            } else {
                let count = {
                    let mut f = self.failures.lock().unwrap();
                    let c = f.entry(sub.id.clone()).or_insert(0);
                    *c += 1;
                    *c
                };
                report.failed.push(sub.id.clone());
                if count >= self.failure_threshold {
                    self.source.disable(&sub.id).await;
                    self.failures.lock().unwrap().remove(&sub.id);
                    report.disabled.push(sub.id);
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"; // awaken-allow: secret (test sample key)

    /// An in-memory [`SubscriptionSource`]: holds one resolved subscription and a
    /// disabled set, so `matching` fences out anything the dispatcher auto-disabled.
    #[derive(Default)]
    struct TestSource {
        subs: Vec<ResolvedSubscription>,
        disabled: Mutex<HashSet<String>>,
    }
    impl TestSource {
        fn with(sub: ResolvedSubscription) -> Arc<Self> {
            Arc::new(Self {
                subs: vec![sub],
                disabled: Mutex::new(HashSet::new()),
            })
        }
        fn is_disabled(&self, id: &str) -> bool {
            self.disabled.lock().unwrap().contains(id)
        }
    }
    #[async_trait]
    impl SubscriptionSource for TestSource {
        async fn matching(&self, _ws: &str, _event: &str) -> Vec<ResolvedSubscription> {
            let disabled = self.disabled.lock().unwrap();
            self.subs
                .iter()
                .filter(|s| !disabled.contains(&s.id))
                .cloned()
                .collect()
        }
        async fn disable(&self, id: &str) {
            self.disabled.lock().unwrap().insert(id.to_string());
        }
    }

    /// A sender scripted to fail the first `fail_n` calls, then succeed.
    struct ScriptedSender {
        calls: Mutex<u32>,
        fail_first: u32,
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

        let report = dispatcher.dispatch(&event(), 1_700_000_000).await;
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
    async fn auto_disables_after_the_failure_threshold() {
        let source = TestSource::with(resolved("wh_1"));
        let sender = ScriptedSender::new(u32::MAX); // always fails
        let dispatcher = WebhookDispatcher::new(source.clone(), sender).with_thresholds(1, 2);

        // First dispatch: 1 consecutive failure — not yet disabled.
        let r1 = dispatcher.dispatch(&event(), 1).await;
        assert_eq!(r1.failed, vec!["wh_1".to_string()]);
        assert!(r1.disabled.is_empty());
        assert!(!source.is_disabled("wh_1"));

        // Second dispatch: crosses the threshold → auto-disabled and fenced out.
        let r2 = dispatcher.dispatch(&event(), 2).await;
        assert_eq!(r2.disabled, vec!["wh_1".to_string()]);
        assert!(source.is_disabled("wh_1"));

        // Now disabled → no longer selected.
        let r3 = dispatcher.dispatch(&event(), 3).await;
        assert!(r3.delivered.is_empty() && r3.failed.is_empty() && r3.disabled.is_empty());
    }
}
