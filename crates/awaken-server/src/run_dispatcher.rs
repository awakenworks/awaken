//! Unified run execution pipeline.
//!
//! Protocol handlers prepare a [`RunSpec`], dispatch it through [`RunDispatcher`],
//! and get back a channel of [`AgentEvent`]s to relay over their transport.

use std::sync::Arc;

use tokio::sync::mpsc;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::message::Message;
use awaken_runtime::AgentRuntime;

use crate::routes::ApiError;
use crate::transport::channel_sink::ChannelEventSink;

/// Validate and normalize run request inputs.
///
/// Checks that messages are non-empty, trims/generates thread_id.
/// Returns `(thread_id, messages)`.
pub fn prepare_run_inputs(
    thread_id: Option<String>,
    messages: Vec<Message>,
) -> Result<(String, Vec<Message>), ApiError> {
    if messages.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one message is required".to_string(),
        ));
    }
    let thread_id = thread_id
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    Ok((thread_id, messages))
}

/// Everything needed to start a run — protocol-agnostic.
pub struct RunSpec {
    pub thread_id: String,
    pub agent_id: Option<String>,
    pub messages: Vec<Message>,
}

/// Unified run execution pipeline.
///
/// Protocol handlers build a [`RunSpec`] from their protocol-specific request,
/// then call [`dispatch`](Self::dispatch) to spawn the runtime and obtain an
/// event receiver.
#[derive(Clone)]
pub struct RunDispatcher {
    runtime: Arc<AgentRuntime>,
}

impl RunDispatcher {
    pub fn new(runtime: Arc<AgentRuntime>) -> Self {
        Self { runtime }
    }

    /// Dispatch a run and return a channel receiver for events.
    ///
    /// Creates an unbounded channel, wraps the sender in a [`ChannelEventSink`],
    /// builds a [`RunRequest`](awaken_runtime::RunRequest) from the spec, and
    /// spawns a background task that drives the runtime.
    pub fn dispatch(&self, spec: RunSpec) -> mpsc::UnboundedReceiver<AgentEvent> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let runtime = self.runtime.clone();
        tokio::spawn(async move {
            let sink = ChannelEventSink::new(event_tx);
            let mut request = awaken_runtime::RunRequest::new(spec.thread_id, spec.messages);
            if let Some(aid) = spec.agent_id {
                request = request.with_agent_id(aid);
            }
            if let Err(e) = runtime.run(request, &sink).await {
                tracing::warn!(error = %e, "run failed");
            }
        });

        event_rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_spec_fields() {
        let spec = RunSpec {
            thread_id: "t-1".into(),
            agent_id: Some("agent-a".into()),
            messages: vec![Message::user("hello")],
        };
        assert_eq!(spec.thread_id, "t-1");
        assert_eq!(spec.agent_id.as_deref(), Some("agent-a"));
        assert_eq!(spec.messages.len(), 1);
    }

    #[test]
    fn run_spec_no_agent() {
        let spec = RunSpec {
            thread_id: "t-2".into(),
            agent_id: None,
            messages: vec![],
        };
        assert!(spec.agent_id.is_none());
    }

    #[test]
    fn prepare_run_inputs_generates_thread_id() {
        let msgs = vec![Message::user("hi")];
        let (thread_id, messages) = prepare_run_inputs(None, msgs).unwrap();
        assert!(!thread_id.is_empty());
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn prepare_run_inputs_uses_provided_thread_id() {
        let msgs = vec![Message::user("hi")];
        let (thread_id, _) = prepare_run_inputs(Some("my-thread".into()), msgs).unwrap();
        assert_eq!(thread_id, "my-thread");
    }

    #[test]
    fn prepare_run_inputs_trims_whitespace() {
        let msgs = vec![Message::user("hi")];
        let (thread_id, _) = prepare_run_inputs(Some("  my-thread  ".into()), msgs).unwrap();
        assert_eq!(thread_id, "my-thread");
    }

    #[test]
    fn prepare_run_inputs_empty_thread_id_generates_new() {
        let msgs = vec![Message::user("hi")];
        let (thread_id, _) = prepare_run_inputs(Some("  ".into()), msgs).unwrap();
        assert!(!thread_id.is_empty());
        assert_ne!(thread_id, "  ");
    }

    #[test]
    fn prepare_run_inputs_empty_messages_errors() {
        let result = prepare_run_inputs(None, vec![]);
        assert!(result.is_err());
    }
}
