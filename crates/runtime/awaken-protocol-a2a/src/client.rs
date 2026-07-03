//! The A2A outbound client: call a *remote* A2A agent.
//!
//! The mirror of the inbound [`router`](crate::router): the same wire types and the
//! same `/v1/a2a/...` paths, in the same bounded context. A caller supplies a
//! [`Transport`] (HTTP, or in-process for tests) and gets neutral `Task` /
//! `AgentCard` results — all A2A protocol knowledge (routes, message shape, the
//! error envelope) stays here. Polling a `working` task to a terminal state and
//! mapping its state to a domain outcome are the *caller's* orchestration, not the
//! wire's.

use async_trait::async_trait;
use serde_json::json;

#[cfg(test)]
use crate::types::TaskState;
use crate::types::{AgentCard, SendMessageResponse, Task};

/// The `message:send` route (shared with the inbound router).
pub const MESSAGE_SEND_PATH: &str = "/v1/a2a/message:send";
/// The agent-card route (shared with the inbound router).
pub const AGENT_CARD_PATH: &str = "/v1/a2a/agent-card";

/// The `tasks/get` route for a task.
pub fn task_path(task_id: &str) -> String {
    format!("/v1/a2a/tasks/{task_id}")
}

/// The `tasks:cancel` route for a task.
pub fn task_cancel_path(task_id: &str) -> String {
    format!("/v1/a2a/tasks/{task_id}:cancel")
}

/// A raw A2A HTTP+JSON response: the status code and the body bytes.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The transport seam for the outbound client: performs an A2A HTTP+JSON request.
/// The composition root supplies the impl (HTTP with credentials, or an in-process
/// router for tests) — the client never names the wire mechanism. A non-2xx status
/// is returned (not raised) so the A2A error envelope reaches the caller.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response, String>;
}

/// A client-side A2A failure (transport error or non-2xx status).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ClientError(pub String);

impl ClientError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

fn ok_status(response: &Response, what: &str) -> Result<(), ClientError> {
    if (200..300).contains(&response.status) {
        Ok(())
    } else {
        Err(ClientError::new(format!(
            "A2A {what} failed: HTTP {}",
            response.status
        )))
    }
}

/// Read a `Task` from a response, accepting either a bare task or a
/// `{ "task": ... }` envelope (`message:send` uses the envelope).
fn read_task(body: &[u8]) -> Result<Task, ClientError> {
    if let Ok(response) = serde_json::from_slice::<SendMessageResponse>(body) {
        return Ok(response.task);
    }
    serde_json::from_slice::<Task>(body).map_err(|e| ClientError::new(e.to_string()))
}

/// Post a `message:send` and return the initial task. `context_id` continues a
/// prior conversation; `message_id` must be unique per message.
pub async fn send_message(
    transport: &dyn Transport,
    agent_id: Option<&str>,
    context_id: &str,
    message_id: &str,
    text: &str,
) -> Result<Task, ClientError> {
    let request = json!({
        "agentId": agent_id,
        "message": {
            "messageId": message_id,
            "contextId": context_id,
            "role": "ROLE_USER",
            "parts": [{ "text": text }],
        }
    });
    let body = serde_json::to_vec(&request).map_err(|e| ClientError::new(e.to_string()))?;
    let response = transport
        .request("POST", MESSAGE_SEND_PATH, Some(body))
        .await
        .map_err(ClientError::new)?;
    ok_status(&response, "message:send")?;
    read_task(&response.body)
}

/// Fetch a task by id (`tasks/get`) — to reattach to or poll an in-flight task.
pub async fn get_task(transport: &dyn Transport, task_id: &str) -> Result<Task, ClientError> {
    let response = transport
        .request("GET", &task_path(task_id), None)
        .await
        .map_err(ClientError::new)?;
    ok_status(&response, "tasks/get")?;
    read_task(&response.body)
}

/// Best-effort cancel of a task (`tasks:cancel`); failures are ignored.
pub async fn cancel_task(transport: &dyn Transport, task_id: &str) {
    let _ = transport
        .request("POST", &task_cancel_path(task_id), None)
        .await;
}

/// Fetch the remote agent's discovery card (`agent-card`).
pub async fn agent_card(transport: &dyn Transport) -> Result<AgentCard, ClientError> {
    let response = transport
        .request("GET", AGENT_CARD_PATH, None)
        .await
        .map_err(ClientError::new)?;
    ok_status(&response, "agent-card")?;
    serde_json::from_slice::<AgentCard>(&response.body).map_err(|e| ClientError::new(e.to_string()))
}

/// A real HTTP [`Transport`] to a remote A2A agent, with an optional bearer token.
/// `ureq` is synchronous, so each request runs on a blocking thread.
pub struct HttpTransport {
    base_url: String,
    bearer: Option<String>,
}

impl HttpTransport {
    /// A transport to the remote agent at `base_url` (e.g. `https://host`); A2A
    /// paths are appended to it.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer: None,
        }
    }

    /// Authenticate every request with `Authorization: Bearer <token>`.
    pub fn with_bearer(mut self, token: impl Into<String>) -> Self {
        self.bearer = Some(token.into());
        self
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response, String> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let method = method.to_string();
        let bearer = self.bearer.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut req = ureq::request(&method, &url);
            if let Some(token) = &bearer {
                req = req.set("authorization", &format!("Bearer {token}"));
            }
            let result = match body {
                Some(bytes) => req
                    .set("content-type", "application/json")
                    .send_bytes(&bytes),
                None => req.call(),
            };
            let (status, response) = match result {
                Ok(response) => (response.status(), response),
                Err(ureq::Error::Status(code, response)) => (code, response),
                Err(err) => return Err(err.to_string()),
            };
            let mut buffer = Vec::new();
            response
                .into_reader()
                .read_to_end(&mut buffer)
                .map_err(|e| e.to_string())?;
            Ok(Response {
                status,
                body: buffer,
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records requests and replies with a queued body, so client wiring (method,
    /// path, parse) is asserted without a socket.
    struct MockTransport {
        seen: Mutex<Vec<(String, String)>>,
        reply: String,
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn request(
            &self,
            method: &str,
            path: &str,
            _body: Option<Vec<u8>>,
        ) -> Result<Response, String> {
            self.seen
                .lock()
                .unwrap()
                .push((method.to_string(), path.to_string()));
            Ok(Response {
                status: 200,
                body: self.reply.clone().into_bytes(),
            })
        }
    }

    #[test]
    fn task_paths_are_built_from_the_id() {
        assert_eq!(task_path("t-1"), "/v1/a2a/tasks/t-1");
        assert_eq!(task_cancel_path("t-1"), "/v1/a2a/tasks/t-1:cancel");
    }

    #[tokio::test]
    async fn send_message_posts_and_parses_the_task() {
        let transport = MockTransport {
            seen: Mutex::new(Vec::new()),
            reply: r#"{"task":{"id":"t-1","contextId":"c","status":{"state":"TASK_STATE_COMPLETED","message":{"messageId":"a","role":"ROLE_AGENT","parts":[{"text":"hi"}]}}}}"#
                .into(),
        };
        let task = send_message(&transport, Some("agent"), "c", "m-1", "go")
            .await
            .unwrap();
        assert_eq!(task.id, "t-1");
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(transport.seen.lock().unwrap()[0].1, MESSAGE_SEND_PATH);
    }

    #[tokio::test]
    async fn get_task_uses_the_task_route() {
        let transport = MockTransport {
            seen: Mutex::new(Vec::new()),
            reply: r#"{"id":"t-9","contextId":"c","status":{"state":"TASK_STATE_WORKING"}}"#.into(),
        };
        let task = get_task(&transport, "t-9").await.unwrap();
        assert_eq!(task.status.state, TaskState::Working);
        let seen = transport.seen.lock().unwrap();
        assert_eq!(
            seen[0],
            ("GET".to_string(), "/v1/a2a/tasks/t-9".to_string())
        );
    }

    #[tokio::test]
    async fn a_non_2xx_status_is_an_error() {
        struct Failing;
        #[async_trait]
        impl Transport for Failing {
            async fn request(
                &self,
                _: &str,
                _: &str,
                _: Option<Vec<u8>>,
            ) -> Result<Response, String> {
                Ok(Response {
                    status: 500,
                    body: b"boom".to_vec(),
                })
            }
        }
        assert!(get_task(&Failing, "t").await.is_err());
    }
}
