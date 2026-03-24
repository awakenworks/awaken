//! Unified run execution pipeline.
//!
//! Protocol handlers prepare a [`RunSpec`], dispatch it through [`RunDispatcher`],
//! and get back a channel of [`AgentEvent`]s to relay over their transport.

use std::sync::Arc;
use std::{collections::HashMap, collections::VecDeque};

use tokio::sync::Mutex;
use tokio::sync::mpsc;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::ToolCallResume;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchStatus {
    Running,
    Queued { pending_ahead: usize },
}

struct QueuedRun {
    spec: RunSpec,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
}

/// Per-thread state: a FIFO queue of pending runs and a flag indicating
/// whether a worker task is currently draining the queue.
#[derive(Default)]
struct ThreadState {
    queue: VecDeque<QueuedRun>,
    worker_active: bool,
}

/// Unified run execution pipeline.
///
/// Protocol handlers build a [`RunSpec`] from their protocol-specific request,
/// then call [`dispatch`](Self::dispatch) to spawn the runtime and obtain an
/// event receiver.
///
/// Delegates cancel and decision operations to the runtime's dual-index
/// `ActiveRunRegistry` (tries run_id first, then thread_id).
#[derive(Clone)]
pub struct RunDispatcher {
    runtime: Arc<AgentRuntime>,
    state: Arc<Mutex<HashMap<String, ThreadState>>>,
}

impl RunDispatcher {
    pub fn new(runtime: Arc<AgentRuntime>) -> Self {
        Self {
            runtime,
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Dispatch a run and return a channel receiver for events.
    ///
    /// Runs are serialized per-thread: at most one active run per thread.
    /// Additional runs for the same thread are queued in FIFO order.
    pub async fn dispatch(&self, spec: RunSpec) -> mpsc::UnboundedReceiver<AgentEvent> {
        self.dispatch_with_status(spec).await.1
    }

    /// Dispatch with queue status, for thread-centric APIs.
    pub async fn dispatch_with_status(
        &self,
        spec: RunSpec,
    ) -> (DispatchStatus, mpsc::UnboundedReceiver<AgentEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let thread_id = spec.thread_id.clone();
        let mut state = self.state.lock().await;
        let ts = state.entry(thread_id.clone()).or_default();
        let status = if !ts.worker_active && ts.queue.is_empty() {
            DispatchStatus::Running
        } else {
            DispatchStatus::Queued {
                pending_ahead: ts.queue.len(),
            }
        };
        let should_spawn = !ts.worker_active;
        ts.queue.push_back(QueuedRun { spec, event_tx });
        if should_spawn {
            ts.worker_active = true;
        }
        drop(state);

        if should_spawn {
            self.spawn_thread_worker(thread_id).await;
        }

        (status, event_rx)
    }

    async fn spawn_thread_worker(&self, thread_id: String) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                // Lock only long enough to pop the next job.
                let next = {
                    let mut state = this.state.lock().await;
                    let Some(ts) = state.get_mut(&thread_id) else {
                        break;
                    };
                    match ts.queue.pop_front() {
                        Some(job) => job,
                        None => {
                            // Queue drained — mark worker inactive and clean up.
                            ts.worker_active = false;
                            if ts.queue.is_empty() {
                                state.remove(&thread_id);
                            }
                            break;
                        }
                    }
                };

                let QueuedRun { spec, event_tx } = next;
                let error_tx = event_tx.clone();
                let sink = ChannelEventSink::new(event_tx);
                let mut request = awaken_runtime::RunRequest::new(spec.thread_id, spec.messages);
                if let Some(aid) = spec.agent_id {
                    request = request.with_agent_id(aid);
                }
                if let Err(e) = this.runtime.run(request, Arc::new(sink)).await {
                    tracing::warn!(error = %e, "run failed");
                    // Notify the caller through the event channel so the
                    // error is not silently swallowed.
                    let _ = error_tx.send(AgentEvent::Error {
                        message: e.to_string(),
                        code: None,
                    });
                }
            }
        });
    }

    /// Cancel an active run. Tries run_id first, then thread_id via dual-index lookup.
    ///
    /// Returns `true` if the cancellation was sent.
    pub fn cancel_run(&self, id: &str) -> bool {
        self.runtime.cancel(id)
    }

    /// Forward a decision to an active run. Tries run_id first, then thread_id.
    ///
    /// Returns `true` if the decision was sent successfully.
    pub fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool {
        self.runtime.send_decision(id, tool_call_id, resume)
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

    #[test]
    fn dispatch_status_enum_smoke() {
        let running = DispatchStatus::Running;
        let queued = DispatchStatus::Queued { pending_ahead: 2 };
        assert!(matches!(running, DispatchStatus::Running));
        assert!(matches!(
            queued,
            DispatchStatus::Queued { pending_ahead: 2 }
        ));
    }
}
