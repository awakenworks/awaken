//! Native reads and replies for the existing profiled Session Run boundary.

use awaken_session_contract::{
    SessionThreadTarget, SessionThreadToolReply, SessionThreadToolReplyCommand,
};
use axum::{
    Router,
    handler::Handler,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

/// Original upstream cursors in their independent durable partitions.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionRunEventsQuery {
    #[serde(default)]
    pub dispatch_after: u64,
    #[serde(default)]
    pub lifecycle_after: u64,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

const fn default_limit() -> usize {
    64
}

/// Exact reply intent; the Session and Run identities come only from the path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionRunResume {
    pub operation_id: String,
    pub expected_thread_version: u64,
    pub correlation_id: String,
    pub call_id: Option<String>,
    pub reply: SessionThreadToolReply,
}

impl PartialEq for ProfiledSessionRunResume {
    fn eq(&self, other: &Self) -> bool {
        // Equality means identical complete reply intent, never just the
        // operation/correlation labels. Durable admission owns the receipt.
        self.operation_id == other.operation_id
            && self.expected_thread_version == other.expected_thread_version
            && self.correlation_id == other.correlation_id
            && self.call_id == other.call_id
            && self.reply == other.reply
    }
}

impl ProfiledSessionRunResume {
    #[must_use]
    pub fn into_command(
        self,
        session_id: awaken_session_types::SessionId,
        run_id: awaken_agent_contract::agent::run::Id,
    ) -> SessionThreadToolReplyCommand {
        SessionThreadToolReplyCommand {
            session_id,
            // This is the neutral occurrence identity, not an authored Managed
            // Event. The existing command owns its complete-payload fingerprint.
            tool_request_event_id: Some(self.operation_id),
            expected_thread_version: Some(self.expected_thread_version),
            target: SessionThreadTarget::Primary,
            expected_run_id: run_id,
            expected_correlation_id: self.correlation_id,
            tool_use_id: self.call_id.unwrap_or_default(),
            reply: self.reply,
            accompanying_system: None,
        }
    }
}

pub fn profiled_session_run_events_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/sessions/{id}/runs/{run}/events", get(handler))
}

pub fn profiled_session_run_resume_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/sessions/{id}/runs/{run}/resume", post(handler))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_reply_replay_retains_its_original_version_and_payload_identity() {
        // Causes: C1 identical operation/ticket/version/reply; C2 one field
        // changes; C3 body tries to supply a parallel Session/Run. Effects:
        // E1 C1 lowers to the same existing durable operation; E2 C2 differs
        // and cannot alias that receipt; E3 C3 is rejected at decode. R1=C1,
        // R2=C2, R3=C3. No current-version read occurs while lowering a retry.
        let body = ProfiledSessionRunResume {
            operation_id: "reply-1".into(),
            expected_thread_version: 17,
            correlation_id: "await-1".into(),
            call_id: None,
            reply: SessionThreadToolReply::Input("answer".into()),
        };
        let run = awaken_agent_contract::agent::run::Id("run-1".into());
        let command = body.clone().into_command("session-1".into(), run.clone());
        let replay = serde_json::from_value::<ProfiledSessionRunResume>(
            serde_json::to_value(&body).unwrap(),
        )
        .unwrap()
        .into_command("session-1".into(), run.clone());
        assert_eq!(command, replay, "R1/E1");
        assert_eq!(
            command.delivery_operation_id(),
            replay.delivery_operation_id()
        );
        for changed in [
            ProfiledSessionRunResume {
                expected_thread_version: 18,
                ..body.clone()
            },
            ProfiledSessionRunResume {
                reply: SessionThreadToolReply::Input("changed".into()),
                ..body.clone()
            },
            ProfiledSessionRunResume {
                operation_id: "reply-2".into(),
                ..body.clone()
            },
        ] {
            assert_ne!(
                command.delivery_operation_id(),
                changed
                    .into_command("session-1".into(), run.clone())
                    .delivery_operation_id(),
                "R2/E2"
            );
        }
        for field in ["session_id", "run_id"] {
            let mut invalid = serde_json::to_value(&body).unwrap();
            invalid[field] = serde_json::json!("foreign");
            assert!(
                serde_json::from_value::<ProfiledSessionRunResume>(invalid).is_err(),
                "R3/E3"
            );
        }
    }

    #[tokio::test]
    async fn native_read_and_reply_share_one_flat_run_path_owner() {
        // R1 GET exact native path => two opaque cursors decoded; R2 POST exact
        // reply path => version retained; R3 wrong method or missing Run path
        // => 405/404. These pin transport only; application/Workspace ownership
        // is covered by Session tests and existing generic Workspace rewriting.
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use axum::{
            Json,
            extract::{Path, Query},
        };
        use tower::ServiceExt;
        async fn read(
            Path((session, run)): Path<(String, String)>,
            Query(query): Query<ProfiledSessionRunEventsQuery>,
        ) -> String {
            format!(
                "{session}:{run}:{}:{}:{}",
                query.dispatch_after, query.lifecycle_after, query.limit
            )
        }
        async fn reply(
            Path((session, run)): Path<(String, String)>,
            Json(body): Json<ProfiledSessionRunResume>,
        ) -> String {
            format!("{session}:{run}:{}", body.expected_thread_version)
        }
        let app = profiled_session_run_events_router(read)
            .merge(profiled_session_run_resume_router(reply));
        let response = app.clone().oneshot(Request::builder()
            .uri("/v1/awaken/sessions/session-1/runs/run-1/events?dispatch_after=7&lifecycle_after=2000&limit=2")
            .body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "R1");
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"session-1:run-1:7:2000:2");
        let body = ProfiledSessionRunResume {
            operation_id: "reply-1".into(),
            expected_thread_version: 17,
            correlation_id: "await-1".into(),
            call_id: None,
            reply: SessionThreadToolReply::Input("answer".into()),
        };
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/awaken/sessions/session-1/runs/run-1/resume")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "R2");
        for (method, path, status) in [
            (
                "POST",
                "/v1/awaken/sessions/session-1/runs/run-1/events",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                "GET",
                "/v1/awaken/sessions/session-1/runs/events",
                StatusCode::NOT_FOUND,
            ),
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(path)
                            .body(Body::empty())
                            .unwrap()
                    )
                    .await
                    .unwrap()
                    .status(),
                status,
                "R3"
            );
        }
    }
}
