//! Routes server notifications to typed sinks.
//!
//! A single background task drains the [`JsonRpcPeer`](crate::jsonrpc::JsonRpcPeer)
//! notification stream and dispatches by method:
//!
//! - `notifications/progress` → the per-call progress channel keyed by token;
//! - `notifications/{tools,prompts,resources}/list_changed` → a list-changed
//!   broadcast (drives dynamic tool refresh);
//! - `notifications/resources/updated` → a resource-updated broadcast.
//!
//! The routing is pure enough to test directly by pushing synthetic
//! notifications, with no subprocess.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::jsonrpc::ServerNotification;
use crate::progress::McpProgressUpdate;
use crate::transport::ListChangedKind;

/// Maximum length of every notification method admitted by this router.
const MAX_NOTIFICATION_METHOD_LEN: usize = "notifications/resources/list_changed".len();

/// Closed set of notification effects the runtime admits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotificationAdmission {
    Progress,
    ToolsListChanged,
    PromptsListChanged,
    ResourcesListChanged,
    ResourceUpdated,
}

/// Classify a wire method before inspecting or dispatching its payload.
///
/// Exact byte matching and the length guard make unknown and extended method
/// names fail closed. `route` is deliberately driven by this classifier so the
/// verified admission decision is the production dispatch boundary.
fn notification_admission(method: &str) -> Option<NotificationAdmission> {
    notification_admission_bytes(method.as_bytes())
}

fn notification_admission_bytes(method: &[u8]) -> Option<NotificationAdmission> {
    if method.len() > MAX_NOTIFICATION_METHOD_LEN {
        return None;
    }
    match method {
        b"notifications/progress" => Some(NotificationAdmission::Progress),
        b"notifications/tools/list_changed" => Some(NotificationAdmission::ToolsListChanged),
        b"notifications/prompts/list_changed" => Some(NotificationAdmission::PromptsListChanged),
        b"notifications/resources/list_changed" => {
            Some(NotificationAdmission::ResourcesListChanged)
        }
        b"notifications/resources/updated" => Some(NotificationAdmission::ResourceUpdated),
        _ => None,
    }
}

/// Per-call progress senders, keyed by the numeric `progressToken` the call put
/// in its `_meta`.
pub(crate) type ProgressRoutes = Arc<Mutex<HashMap<i64, mpsc::Sender<McpProgressUpdate>>>>;

/// The typed sinks a [`route`] call dispatches into.
pub(crate) struct NotificationSinks {
    pub(crate) progress: ProgressRoutes,
    pub(crate) list_changed: broadcast::Sender<ListChangedKind>,
    pub(crate) resource_updated: broadcast::Sender<String>,
}

impl NotificationSinks {
    pub(crate) fn new() -> Self {
        Self {
            progress: Arc::new(Mutex::new(HashMap::new())),
            list_changed: broadcast::channel(64).0,
            resource_updated: broadcast::channel(64).0,
        }
    }
}

/// Spawn the background task that drains `notifications` into `sinks`.
pub(crate) fn spawn_router(
    mut notifications: mpsc::Receiver<ServerNotification>,
    sinks: Arc<NotificationSinks>,
) {
    tokio::spawn(async move {
        while let Some(notification) = notifications.recv().await {
            route(notification, &sinks).await;
        }
    });
}

/// Parse and dispatch one notification. Unknown methods and unroutable payloads
/// are dropped silently.
pub(crate) async fn route(notification: ServerNotification, sinks: &NotificationSinks) {
    match notification_admission(&notification.method) {
        Some(NotificationAdmission::Progress) => {
            if let Some((token, update)) = parse_progress(&notification.params) {
                let route = sinks.progress.lock().await.get(&token).cloned();
                if let Some(tx) = route {
                    let _ = tx.send(update).await;
                }
            }
        }
        Some(NotificationAdmission::ToolsListChanged) => {
            let _ = sinks.list_changed.send(ListChangedKind::Tools);
        }
        Some(NotificationAdmission::PromptsListChanged) => {
            let _ = sinks.list_changed.send(ListChangedKind::Prompts);
        }
        Some(NotificationAdmission::ResourcesListChanged) => {
            let _ = sinks.list_changed.send(ListChangedKind::Resources);
        }
        Some(NotificationAdmission::ResourceUpdated) => {
            if let Some(uri) = notification.params.get("uri").and_then(Value::as_str) {
                let _ = sinks.resource_updated.send(uri.to_string());
            }
        }
        None => {}
    }
}

/// Parse a `notifications/progress` payload into its token and update.
pub(crate) fn parse_progress(params: &Value) -> Option<(i64, McpProgressUpdate)> {
    let token = params.get("progressToken")?.as_i64()?;
    let progress = params.get("progress")?.as_f64()?;
    let total = params.get("total").and_then(Value::as_f64);
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((
        token,
        McpProgressUpdate {
            progress,
            total,
            message,
        },
    ))
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn notification_admission_is_exact_and_unknown_fails_closed() {
        let bytes: [u8; MAX_NOTIFICATION_METHOD_LEN] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= MAX_NOTIFICATION_METHOD_LEN);
        let method = &bytes[..len];

        match notification_admission_bytes(method) {
            Some(NotificationAdmission::Progress) => {
                assert_eq!(method, b"notifications/progress")
            }
            Some(NotificationAdmission::ToolsListChanged) => {
                assert_eq!(method, b"notifications/tools/list_changed")
            }
            Some(NotificationAdmission::PromptsListChanged) => {
                assert_eq!(method, b"notifications/prompts/list_changed")
            }
            Some(NotificationAdmission::ResourcesListChanged) => {
                assert_eq!(method, b"notifications/resources/list_changed")
            }
            Some(NotificationAdmission::ResourceUpdated) => {
                assert_eq!(method, b"notifications/resources/updated")
            }
            None => assert!(
                method != b"notifications/progress"
                    && method != b"notifications/tools/list_changed"
                    && method != b"notifications/prompts/list_changed"
                    && method != b"notifications/resources/list_changed"
                    && method != b"notifications/resources/updated"
            ),
        }

        let too_long = [0_u8; MAX_NOTIFICATION_METHOD_LEN + 1];
        assert_eq!(notification_admission_bytes(&too_long), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(method: &str, params: Value) -> ServerNotification {
        ServerNotification {
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn parse_progress_reads_token_and_fields() {
        let (token, update) = parse_progress(&serde_json::json!({
            "progressToken": 5, "progress": 2.0, "total": 4.0, "message": "half"
        }))
        .expect("parses");
        assert_eq!(token, 5);
        assert_eq!(update.progress, 2.0);
        assert_eq!(update.total, Some(4.0));
        assert_eq!(update.message.as_deref(), Some("half"));
    }

    #[test]
    fn parse_progress_without_token_is_none() {
        assert!(parse_progress(&serde_json::json!({ "progress": 1.0 })).is_none());
    }

    #[test]
    fn admission_is_exact_and_extended_names_are_unknown() {
        assert_eq!(
            notification_admission("notifications/progress"),
            Some(NotificationAdmission::Progress)
        );
        assert_eq!(
            notification_admission("notifications/progress/extended"),
            None
        );
        assert_eq!(notification_admission("notifications/unknown"), None);
    }

    #[tokio::test]
    async fn progress_routes_to_the_registered_token() {
        let sinks = NotificationSinks::new();
        let (tx, mut rx) = mpsc::channel(4);
        sinks.progress.lock().await.insert(9, tx);

        route(
            notification(
                "notifications/progress",
                serde_json::json!({ "progressToken": 9, "progress": 0.5 }),
            ),
            &sinks,
        )
        .await;

        let update = rx.recv().await.expect("routed");
        assert_eq!(update.progress, 0.5);
    }

    #[tokio::test]
    async fn progress_for_an_unknown_token_is_dropped() {
        let sinks = NotificationSinks::new();
        // No registered token: routing must not panic and there is nothing to
        // receive.
        route(
            notification(
                "notifications/progress",
                serde_json::json!({ "progressToken": 1, "progress": 0.1 }),
            ),
            &sinks,
        )
        .await;
    }

    #[tokio::test]
    async fn list_changed_broadcasts_by_kind() {
        let sinks = NotificationSinks::new();
        let mut rx = sinks.list_changed.subscribe();
        route(
            notification("notifications/tools/list_changed", Value::Null),
            &sinks,
        )
        .await;
        assert_eq!(rx.recv().await.unwrap(), ListChangedKind::Tools);

        route(
            notification("notifications/resources/list_changed", Value::Null),
            &sinks,
        )
        .await;
        assert_eq!(rx.recv().await.unwrap(), ListChangedKind::Resources);
    }

    #[tokio::test]
    async fn resource_updated_broadcasts_the_uri() {
        let sinks = NotificationSinks::new();
        let mut rx = sinks.resource_updated.subscribe();
        route(
            notification(
                "notifications/resources/updated",
                serde_json::json!({ "uri": "file:///x" }),
            ),
            &sinks,
        )
        .await;
        assert_eq!(rx.recv().await.unwrap(), "file:///x");
    }

    #[tokio::test]
    async fn unknown_notification_is_ignored() {
        let sinks = NotificationSinks::new();
        route(notification("notifications/unknown", Value::Null), &sinks).await;
    }
}
