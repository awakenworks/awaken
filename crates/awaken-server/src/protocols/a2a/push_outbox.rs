use async_trait::async_trait;
use awaken_protocol_a2a::{PushNotificationConfig, StreamResponse};
use awaken_server_contract::contract::outbox::{
    OUTBOX_LANE_PROTOCOL_REPLAY, OUTBOX_TARGET_A2A_WEBHOOK, OutboxError, OutboxMessage,
    OutboxMessageDraft, OutboxStore,
};
use serde::{Deserialize, Serialize};

use crate::outbox_relay::{OutboxRelayError, OutboxRelayHandler};

use super::types::A2A_NOTIFICATION_TOKEN_HEADER;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct A2aPushWebhookPayload {
    pub config: PushNotificationConfig,
    pub response: StreamResponse,
}

pub(crate) async fn enqueue_push_notification(
    outbox: &dyn OutboxStore,
    config: &PushNotificationConfig,
    response: &StreamResponse,
) -> Result<(), OutboxError> {
    let draft = OutboxMessageDraft::new(
        OUTBOX_LANE_PROTOCOL_REPLAY,
        OUTBOX_TARGET_A2A_WEBHOOK,
        serde_json::to_value(A2aPushWebhookPayload {
            config: config.clone(),
            response: response.clone(),
        })
        .map_err(|error| OutboxError::Serialization(error.to_string()))?,
    )?;
    outbox.enqueue_outbox(draft).await.map(|_| ())
}

pub(crate) struct A2aPushWebhookRelayHandler {
    client: reqwest::Client,
}

impl A2aPushWebhookRelayHandler {
    #[must_use]
    pub(crate) fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl OutboxRelayHandler for A2aPushWebhookRelayHandler {
    async fn deliver(&self, message: &OutboxMessage) -> Result<(), OutboxRelayError> {
        validate_a2a_webhook_route(message)?;
        let payload: A2aPushWebhookPayload = serde_json::from_value(message.payload.clone())
            .map_err(|error| OutboxRelayError::Validation(error.to_string()))?;
        post_push_notification(&self.client, &payload.config, &payload.response).await
    }
}

fn validate_a2a_webhook_route(message: &OutboxMessage) -> Result<(), OutboxRelayError> {
    if message.lane == OUTBOX_LANE_PROTOCOL_REPLAY && message.target == OUTBOX_TARGET_A2A_WEBHOOK {
        return Ok(());
    }
    Err(OutboxRelayError::Validation(format!(
        "unexpected outbox message route: lane={}, target={}",
        message.lane, message.target
    )))
}

async fn post_push_notification(
    client: &reqwest::Client,
    config: &PushNotificationConfig,
    payload: &StreamResponse,
) -> Result<(), OutboxRelayError> {
    let mut request = client.post(&config.url).json(payload);
    if let Some(token) = config.token.as_deref() {
        request = request.header(A2A_NOTIFICATION_TOKEN_HEADER, token);
    }
    if let Some(authentication) = config.authentication.as_ref() {
        let credentials = authentication.credentials.as_deref().unwrap_or_default();
        request = request.header(
            reqwest::header::AUTHORIZATION,
            format!("{} {}", authentication.scheme, credentials).trim(),
        );
    }

    let response = request
        .send()
        .await
        .map_err(|error| OutboxRelayError::Delivery(error.to_string()))?;
    if response.status().is_success() {
        return Ok(());
    }
    Err(OutboxRelayError::Delivery(format!(
        "A2A push notification webhook returned {} for {}",
        response.status(),
        config.url
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_protocol_a2a::{AuthenticationInfo, TaskState, TaskStatus, TaskStatusUpdateEvent};
    use awaken_server_contract::contract::outbox::{OutboxMessage, OutboxStatus};
    use awaken_stores::InMemoryOutboxStore;
    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tokio::sync::{Mutex, oneshot};

    use super::*;

    type Capture = Arc<Mutex<Option<(HeaderMap, Value)>>>;

    fn response() -> StreamResponse {
        StreamResponse {
            status_update: Some(TaskStatusUpdateEvent {
                task_id: "task_1".into(),
                context_id: "thread_1".into(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            }),
            ..Default::default()
        }
    }

    fn config(url: String) -> PushNotificationConfig {
        PushNotificationConfig {
            agent_id: None,
            id: Some("push_1".into()),
            task_id: Some("task_1".into()),
            url,
            token: Some("token-1".into()),
            authentication: Some(AuthenticationInfo {
                scheme: "Bearer".into(),
                credentials: Some("secret".into()),
            }),
        }
    }

    async fn webhook() -> (String, Capture, oneshot::Sender<()>) {
        let capture = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route(
                "/hook",
                post(
                    |State(capture): State<Capture>, headers: HeaderMap, body: String| async move {
                        let value = serde_json::from_str::<Value>(&body).unwrap();
                        *capture.lock().await = Some((headers, value));
                    },
                ),
            )
            .with_state(capture.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        (format!("http://{addr}/hook"), capture, shutdown_tx)
    }

    #[tokio::test]
    async fn enqueue_push_notification_writes_webhook_outbox_message() {
        let outbox = InMemoryOutboxStore::new();
        enqueue_push_notification(
            &outbox,
            &config("http://127.0.0.1/hook".into()),
            &response(),
        )
        .await
        .unwrap();

        let pending = outbox
            .list_outbox(Some(OutboxStatus::Pending), 10)
            .await
            .unwrap();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].lane, OUTBOX_LANE_PROTOCOL_REPLAY);
        assert_eq!(pending[0].target, OUTBOX_TARGET_A2A_WEBHOOK);
        let payload: A2aPushWebhookPayload =
            serde_json::from_value(pending[0].payload.clone()).unwrap();
        assert_eq!(payload.response, response());
    }

    #[tokio::test]
    async fn relay_posts_webhook_payload_with_auth_headers() {
        let (url, capture, shutdown) = webhook().await;
        let outbox = InMemoryOutboxStore::new();
        let cfg = config(url);
        enqueue_push_notification(&outbox, &cfg, &response())
            .await
            .unwrap();
        let mut messages = outbox
            .claim_outbox(
                OUTBOX_LANE_PROTOCOL_REPLAY,
                OUTBOX_TARGET_A2A_WEBHOOK,
                1,
                30_000,
                "test",
                1,
            )
            .await
            .unwrap();
        let message = messages.pop().unwrap();
        A2aPushWebhookRelayHandler::new(reqwest::Client::new())
            .deliver(&message)
            .await
            .unwrap();

        let captured = capture.lock().await.clone().unwrap();

        assert_eq!(
            captured.0.get(A2A_NOTIFICATION_TOKEN_HEADER).unwrap(),
            "token-1"
        );
        assert_eq!(
            captured.0.get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer secret"
        );
        assert_eq!(captured.1["statusUpdate"]["taskId"], "task_1");
        let _ = shutdown.send(());
    }

    #[test]
    fn relay_rejects_wrong_outbox_route() {
        let message = OutboxMessage::from_enqueue(
            "out_1".into(),
            OutboxMessageDraft::new("canonical", "other", serde_json::json!({})).unwrap(),
            1,
        )
        .unwrap();

        let err = validate_a2a_webhook_route(&message).unwrap_err();

        assert!(matches!(err, OutboxRelayError::Validation(_)));
    }
}
