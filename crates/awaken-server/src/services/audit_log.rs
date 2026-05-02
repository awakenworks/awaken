//! Audit log service — stores, queries, and prunes audit events.
//!
//! Events are stored in the `_audit` namespace of the existing `ConfigStore`
//! using ULID keys so time-ordering is preserved without a secondary index.

use std::sync::Arc;

use awaken_contract::AuditAction;
use awaken_contract::AuditEvent;
use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::storage::StorageError;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::Digest;

/// Storage namespace for all audit events.
pub const AUDIT_NAMESPACE: &str = "_audit";

/// Query parameters for listing audit events.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AuditQuery {
    /// Include only events at or after this timestamp (RFC 3339).
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// Include only events strictly before this timestamp (RFC 3339).
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    /// Filter by action type.
    #[serde(default)]
    pub action: Option<AuditAction>,
    /// Filter by exact resource path (`namespace/id`).
    #[serde(default)]
    pub resource: Option<String>,
    /// Filter by actor hash prefix.
    #[serde(default)]
    pub actor: Option<String>,
    /// Max results, default 100, capped at 1000.
    #[serde(default = "default_audit_limit")]
    pub limit: usize,
    /// Opaque keyset cursor from a previous response.
    #[serde(default)]
    pub cursor: Option<String>,
}

fn default_audit_limit() -> usize {
    100
}

/// Paginated result set for audit event queries.
#[derive(Debug, serde::Serialize)]
pub struct AuditPage {
    pub items: Vec<AuditEvent>,
    pub next_cursor: Option<String>,
}

/// Error returned by [`AuditLogger::query`].
#[derive(Debug, thiserror::Error)]
pub enum AuditQueryError {
    #[error("invalid cursor")]
    InvalidCursor,
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Stateless service that records and queries audit events.
pub struct AuditLogger {
    store: Arc<dyn ConfigStore>,
}

impl AuditLogger {
    pub fn new(store: Arc<dyn ConfigStore>) -> Self {
        Self { store }
    }

    /// Record an audit event.  Best-effort: failures emit a warning and
    /// increment the write-failure metric but are never propagated to callers.
    pub async fn emit(
        &self,
        action: AuditAction,
        resource: &str,
        before: Option<Value>,
        after: Option<Value>,
        headers: &HeaderMap,
    ) {
        let id = ulid::Ulid::new().to_string();
        let ts = Utc::now().to_rfc3339();
        let actor = derive_actor(headers);
        let ip = extract_client_ip(headers);
        let request_id = headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let before = before.map(redact_secrets);
        let after = after.map(redact_secrets);

        let event = AuditEvent {
            id: id.clone(),
            ts,
            actor,
            action,
            resource: resource.to_string(),
            before,
            after,
            ip,
            request_id,
            restored_from: None,
        };

        let value = match serde_json::to_value(&event) {
            Ok(v) => v,
            Err(error) => {
                tracing::warn!(error = %error, "audit: failed to serialize event");
                metrics::counter!("awaken_audit_write_failures_total").increment(1);
                return;
            }
        };

        if let Err(error) = self.store.put(AUDIT_NAMESPACE, &id, &value).await {
            tracing::warn!(error = %error, "audit: failed to write event");
            metrics::counter!("awaken_audit_write_failures_total").increment(1);
            return;
        }

        let action_label = serde_json::to_value(&event.action)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string());
        metrics::counter!("awaken_audit_events_total", "action" => action_label).increment(1);
    }

    /// Look up a single audit event by its ULID id.
    ///
    /// Returns `Ok(None)` when the event is not found (either never existed or was pruned).
    pub async fn get_event(&self, id: &str) -> Result<Option<AuditEvent>, StorageError> {
        let value = self.store.get(AUDIT_NAMESPACE, id).await?;
        Ok(value.and_then(|v| serde_json::from_value::<AuditEvent>(v).ok()))
    }

    /// Emit a restore audit event with the `restored_from` field set.
    ///
    /// Best-effort — same semantics as [`AuditLogger::emit`].
    pub async fn emit_restore(
        &self,
        resource: &str,
        before: Option<Value>,
        after: Option<Value>,
        restored_from: String,
        headers: &HeaderMap,
    ) {
        let id = ulid::Ulid::new().to_string();
        let ts = Utc::now().to_rfc3339();
        let actor = derive_actor(headers);
        let ip = extract_client_ip(headers);
        let request_id = headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let before = before.map(redact_secrets);
        let after = after.map(redact_secrets);

        let event = AuditEvent {
            id: id.clone(),
            ts,
            actor,
            action: AuditAction::Restore,
            resource: resource.to_string(),
            before,
            after,
            ip,
            request_id,
            restored_from: Some(restored_from),
        };

        let value = match serde_json::to_value(&event) {
            Ok(v) => v,
            Err(error) => {
                tracing::warn!(error = %error, "audit: failed to serialize restore event");
                metrics::counter!("awaken_audit_write_failures_total").increment(1);
                return;
            }
        };

        if let Err(error) = self.store.put(AUDIT_NAMESPACE, &id, &value).await {
            tracing::warn!(error = %error, "audit: failed to write restore event");
            metrics::counter!("awaken_audit_write_failures_total").increment(1);
            return;
        }

        metrics::counter!("awaken_audit_events_total", "action" => "restore").increment(1);
    }

    /// Query audit events with optional filters and keyset pagination.
    ///
    /// Returns `Err` if `filter.cursor` is present but not valid base64.
    pub async fn query(&self, filter: AuditQuery) -> Result<AuditPage, AuditQueryError> {
        let effective_limit = filter.limit.clamp(1, 1000);

        // Decode cursor to get the last-seen id (exclusive upper bound).
        let cursor_id = filter
            .cursor
            .as_deref()
            .map(decode_cursor)
            .transpose()
            .map_err(|_| AuditQueryError::InvalidCursor)?;

        // Fetch all entries in the namespace (sorted ascending by ULID).
        // Linear scan per ADR D3 footnote.
        let all = self
            .store
            .list(AUDIT_NAMESPACE, 0, usize::MAX)
            .await
            .map_err(AuditQueryError::Storage)?;

        // Deserialize and filter.
        let mut events: Vec<AuditEvent> = all
            .into_iter()
            .filter_map(|(id, value)| {
                // Cursor: only include entries with id < cursor_id (older pages).
                // We're serving newest-first so we reverse later.
                if cursor_id.as_deref().is_some_and(|cid| id.as_str() >= cid) {
                    return None;
                }
                serde_json::from_value::<AuditEvent>(value).ok()
            })
            .filter(|event| {
                if let Some(ref since) = filter.since
                    && let Ok(ts) = event.ts.parse::<DateTime<Utc>>()
                    && ts < *since
                {
                    return false;
                }
                if let Some(ref until) = filter.until
                    && let Ok(ts) = event.ts.parse::<DateTime<Utc>>()
                    && ts >= *until
                {
                    return false;
                }
                if let Some(ref action) = filter.action
                    && &event.action != action
                {
                    return false;
                }
                if let Some(ref resource) = filter.resource
                    && &event.resource != resource
                {
                    return false;
                }
                if let Some(ref actor) = filter.actor
                    && !event.actor.starts_with(actor.as_str())
                {
                    return false;
                }
                true
            })
            .collect();

        // Newest first.
        events.sort_by(|a, b| b.id.cmp(&a.id));

        let next_cursor = if events.len() > effective_limit {
            events.truncate(effective_limit);
            events.last().map(|e| encode_cursor(&e.id))
        } else {
            None
        };

        Ok(AuditPage {
            items: events,
            next_cursor,
        })
    }

    /// Delete all events whose ULID timestamp is before `cutoff`.
    /// Returns the number of pruned entries.
    pub async fn prune_before(&self, cutoff: DateTime<Utc>) -> Result<usize, StorageError> {
        let all = self.store.list(AUDIT_NAMESPACE, 0, usize::MAX).await?;

        let mut pruned = 0usize;
        for (id, _) in all {
            // Decode ULID timestamp.
            if let Ok(ulid) = id.parse::<ulid::Ulid>() {
                let ms = ulid.timestamp_ms();
                let event_ts =
                    DateTime::from_timestamp_millis(ms as i64).unwrap_or(DateTime::UNIX_EPOCH);
                if event_ts < cutoff {
                    self.store.delete(AUDIT_NAMESPACE, &id).await?;
                    pruned += 1;
                }
            }
        }

        if pruned > 0 {
            metrics::counter!("awaken_audit_sweep_pruned_total").increment(pruned as u64);
            tracing::info!(pruned, "audit sweep pruned events");
        }
        Ok(pruned)
    }
}

/// Derive the actor string from request headers.
///
/// - `Authorization: Bearer <token>` → first 16 hex chars of SHA-256(token)
/// - Otherwise → `"anonymous"`
/// - If `X-Awaken-Actor` is also present and valid → append `"/<label>"`
pub fn derive_actor(headers: &HeaderMap) -> String {
    let base = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .map(|token| {
            let hash = sha2::Sha256::digest(token.as_bytes());
            let hex = format!("{hash:x}");
            hex[..16].to_string()
        })
        .unwrap_or_else(|| "anonymous".to_string());

    // Optional advisory label.
    if let Some(label) = headers
        .get("x-awaken-actor")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter(|s| s.len() <= 64)
        .filter(|s| s.bytes().all(|b| b.is_ascii() && !b.is_ascii_control()))
    {
        format!("{base}/{label}")
    } else {
        base
    }
}

/// Recursively replace values whose key matches common secret patterns with `"***"`.
pub fn redact_secrets(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, val) in map {
                let lower = key.to_lowercase();
                if lower.contains("api_key")
                    || lower.contains("bearer")
                    || lower.contains("token")
                    || lower.contains("password")
                    || lower.contains("secret")
                {
                    out.insert(key, Value::String("***".to_string()));
                } else {
                    out.insert(key, redact_secrets(val));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(redact_secrets).collect()),
        other => other,
    }
}

/// Extract the client IP from request headers.
/// Prefers `x-forwarded-for` (first address); falls back to `x-real-ip`.
pub fn extract_client_ip(headers: &HeaderMap) -> Option<String> {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let first = xff.split(',').next().map(str::trim).unwrap_or("");
        if !first.is_empty() {
            return Some(first.to_string());
        }
    }
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn encode_cursor(id: &str) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, id)
}

fn decode_cursor(cursor: &str) -> Result<String, ()> {
    base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, cursor)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use awaken_contract::AuditAction;
    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_contract::contract::storage::StorageError;
    use axum::http::{HeaderMap, HeaderValue};
    use chrono::Utc;
    use serde_json::{Value, json};
    use tokio::sync::RwLock;

    use super::*;

    // ── minimal in-memory store ───────────────────────────────────────────

    #[derive(Default)]
    struct MemStore {
        data: RwLock<HashMap<String, HashMap<String, Value>>>,
    }

    #[async_trait]
    impl ConfigStore for MemStore {
        async fn get(&self, ns: &str, id: &str) -> Result<Option<Value>, StorageError> {
            Ok(self
                .data
                .read()
                .await
                .get(ns)
                .and_then(|m| m.get(id))
                .cloned())
        }

        async fn list(
            &self,
            ns: &str,
            _offset: usize,
            _limit: usize,
        ) -> Result<Vec<(String, Value)>, StorageError> {
            let data = self.data.read().await;
            let mut items: Vec<_> = data
                .get(ns)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(items)
        }

        async fn put(&self, ns: &str, id: &str, value: &Value) -> Result<(), StorageError> {
            self.data
                .write()
                .await
                .entry(ns.to_string())
                .or_default()
                .insert(id.to_string(), value.clone());
            Ok(())
        }

        async fn delete(&self, ns: &str, id: &str) -> Result<(), StorageError> {
            if let Some(m) = self.data.write().await.get_mut(ns) {
                m.remove(id);
            }
            Ok(())
        }
    }

    fn make_logger() -> AuditLogger {
        AuditLogger::new(Arc::new(MemStore::default()))
    }

    fn empty_headers() -> HeaderMap {
        HeaderMap::new()
    }

    // ── derive_actor ──────────────────────────────────────────────────────

    #[test]
    fn derive_actor_anonymous_when_no_auth() {
        let headers = empty_headers();
        assert_eq!(derive_actor(&headers), "anonymous");
    }

    #[test]
    fn derive_actor_hash_only_with_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer mysecrettoken"),
        );
        let actor = derive_actor(&headers);
        assert!(
            !actor.contains("mysecrettoken"),
            "raw token must not appear"
        );
        assert_eq!(actor.len(), 16, "hash prefix must be 16 hex chars");
        assert!(actor.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn derive_actor_hash_plus_valid_label() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer tok"),
        );
        headers.insert("x-awaken-actor", HeaderValue::from_static("ci/deploy-prod"));
        let actor = derive_actor(&headers);
        assert!(actor.contains("/ci/deploy-prod"), "label must be appended");
    }

    #[test]
    fn derive_actor_invalid_label_dropped() {
        // Verify that a label containing non-printable ASCII (control chars) is
        // dropped at the derive_actor logic level. HTTP headers cannot carry
        // control bytes, so we test the filter predicate directly by injecting
        // a mock value that passes header parsing but fails our check.
        // We approximate this by testing empty label (trimmed to empty → dropped).
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer tok"),
        );
        headers.insert("x-awaken-actor", HeaderValue::from_static("   "));
        let actor = derive_actor(&headers);
        // A whitespace-only label trims to empty → dropped.
        assert!(
            !actor.contains('/'),
            "empty/whitespace label must not be appended"
        );
        assert_eq!(actor.len(), 16);
    }

    #[test]
    fn derive_actor_label_too_long_dropped() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer tok"),
        );
        let long_label = "a".repeat(65);
        headers.insert(
            "x-awaken-actor",
            HeaderValue::from_str(&long_label).unwrap(),
        );
        let actor = derive_actor(&headers);
        assert!(
            !actor.contains('/'),
            "over-length label must not be appended"
        );
    }

    // ── redact_secrets ────────────────────────────────────────────────────

    #[test]
    fn redact_secrets_top_level() {
        let input = json!({"api_key": "sk-1234", "name": "test"});
        let output = redact_secrets(input);
        assert_eq!(output["api_key"], "***");
        assert_eq!(output["name"], "test");
    }

    #[test]
    fn redact_secrets_nested_objects() {
        let input = json!({"provider": {"api_key": "sk-1234", "model": "gpt-4"}});
        let output = redact_secrets(input);
        assert_eq!(output["provider"]["api_key"], "***");
        assert_eq!(output["provider"]["model"], "gpt-4");
    }

    #[test]
    fn redact_secrets_arrays_of_objects() {
        let input = json!([{"password": "hunter2", "user": "alice"}]);
        let output = redact_secrets(input);
        assert_eq!(output[0]["password"], "***");
        assert_eq!(output[0]["user"], "alice");
    }

    #[test]
    fn redact_secrets_mixed_primitives() {
        let input = json!({"count": 42, "flag": true, "nothing": null, "secret": "x"});
        let output = redact_secrets(input);
        assert_eq!(output["count"], 42);
        assert_eq!(output["flag"], true);
        assert_eq!(output["nothing"], Value::Null);
        assert_eq!(output["secret"], "***");
    }

    // ── emit ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn emit_happy_path_stores_event() {
        let logger = make_logger();
        let headers = empty_headers();
        logger
            .emit(
                AuditAction::Create,
                "agents/my-agent",
                None,
                Some(json!({"id": "my-agent"})),
                &headers,
            )
            .await;

        let page = logger.query(AuditQuery::default()).await.unwrap();
        assert_eq!(page.items.len(), 1);
        let event = &page.items[0];
        assert_eq!(event.action, AuditAction::Create);
        assert_eq!(event.resource, "agents/my-agent");
        assert_eq!(event.actor, "anonymous");
    }

    #[tokio::test]
    async fn emit_failure_does_not_propagate() {
        // Use a store that always fails writes.
        struct FailStore;

        #[async_trait]
        impl ConfigStore for FailStore {
            async fn get(&self, _ns: &str, _id: &str) -> Result<Option<Value>, StorageError> {
                Ok(None)
            }
            async fn list(
                &self,
                _ns: &str,
                _offset: usize,
                _limit: usize,
            ) -> Result<Vec<(String, Value)>, StorageError> {
                Ok(vec![])
            }
            async fn put(&self, _ns: &str, _id: &str, _value: &Value) -> Result<(), StorageError> {
                Err(StorageError::Io("simulated failure".into()))
            }
            async fn delete(&self, _ns: &str, _id: &str) -> Result<(), StorageError> {
                Ok(())
            }
        }

        let logger = AuditLogger::new(Arc::new(FailStore));
        // Must not panic or propagate.
        logger
            .emit(
                AuditAction::Delete,
                "agents/x",
                None,
                None,
                &empty_headers(),
            )
            .await;
    }

    // ── query filters ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn query_filters_by_resource() {
        let logger = make_logger();
        let h = empty_headers();
        logger
            .emit(AuditAction::Create, "agents/a", None, None, &h)
            .await;
        logger
            .emit(AuditAction::Create, "agents/b", None, None, &h)
            .await;

        let page = logger
            .query(AuditQuery {
                resource: Some("agents/a".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].resource, "agents/a");
    }

    #[tokio::test]
    async fn query_filters_by_action() {
        let logger = make_logger();
        let h = empty_headers();
        logger
            .emit(AuditAction::Create, "agents/c", None, None, &h)
            .await;
        logger
            .emit(AuditAction::Delete, "agents/c", None, None, &h)
            .await;

        let page = logger
            .query(AuditQuery {
                action: Some(AuditAction::Delete),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].action, AuditAction::Delete);
    }

    // ── cursor pagination ─────────────────────────────────────────────────

    #[tokio::test]
    async fn cursor_pagination_round_trip() {
        let logger = make_logger();
        let h = empty_headers();

        // Emit 5 events.
        for i in 0..5 {
            logger
                .emit(
                    AuditAction::Create,
                    &format!("agents/agent-{i}"),
                    None,
                    None,
                    &h,
                )
                .await;
            // Tiny sleep to ensure distinct ULIDs.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        // Page 1: limit 3.
        let page1 = logger
            .query(AuditQuery {
                limit: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page1.items.len(), 3);
        assert!(page1.next_cursor.is_some());

        // Page 2: continue with cursor.
        let page2 = logger
            .query(AuditQuery {
                limit: 3,
                cursor: page1.next_cursor,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page2.items.len(), 2);
        assert!(page2.next_cursor.is_none());
    }

    // ── prune_before ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn prune_before_removes_old_events() {
        let logger = make_logger();
        let h = empty_headers();
        logger
            .emit(AuditAction::Create, "agents/old", None, None, &h)
            .await;

        let cutoff = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        logger
            .emit(AuditAction::Create, "agents/new", None, None, &h)
            .await;

        let pruned = logger.prune_before(cutoff).await.unwrap();
        assert_eq!(pruned, 1, "one old event should be pruned");

        let page = logger.query(AuditQuery::default()).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].resource, "agents/new");
    }
}
