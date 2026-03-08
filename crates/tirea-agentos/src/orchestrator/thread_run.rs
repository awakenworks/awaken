use super::AgentOs;
use crate::contracts::ToolCallDecision;
use crate::runtime::loop_runner::RunCancellationToken;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};

/// Unified per-active-run handle managed by AgentOS.
#[derive(Clone)]
pub struct ThreadRunHandle {
    thread_id: String,
    run_id: String,
    agent_id: String,
    cancellation_token: RunCancellationToken,
    decision_tx: Arc<RwLock<Option<mpsc::UnboundedSender<ToolCallDecision>>>>,
    pending_decisions: Arc<Mutex<Vec<ToolCallDecision>>>,
    stream_fanout: Arc<RwLock<Option<broadcast::Sender<Bytes>>>>,
}

impl ThreadRunHandle {
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn cancellation_token(&self) -> RunCancellationToken {
        self.cancellation_token.clone()
    }

    pub fn can_own(&self, agent_id: &str) -> bool {
        self.agent_id == agent_id
    }

    pub async fn bind_stream_fanout(&self, fanout: broadcast::Sender<Bytes>) {
        *self.stream_fanout.write().await = Some(fanout);
    }

    pub async fn subscribe_stream_fanout(&self) -> Option<broadcast::Receiver<Bytes>> {
        let fanout = self.stream_fanout.read().await;
        fanout.as_ref().map(|sender| sender.subscribe())
    }

    pub async fn bind_decision_tx(&self, tx: mpsc::UnboundedSender<ToolCallDecision>) -> bool {
        let pending = {
            let mut queued = self.pending_decisions.lock().await;
            std::mem::take(&mut *queued)
        };
        for decision in &pending {
            if tx.send(decision.clone()).is_err() {
                return false;
            }
        }
        *self.decision_tx.write().await = Some(tx);
        true
    }

    pub async fn send_decisions(&self, decisions: &[ToolCallDecision]) -> bool {
        let decision_tx = {
            let guard = self.decision_tx.read().await;
            guard.clone()
        };
        let Some(decision_tx) = decision_tx else {
            let mut queued = self.pending_decisions.lock().await;
            queued.extend_from_slice(decisions);
            return true;
        };
        for decision in decisions {
            if decision_tx.send(decision.clone()).is_err() {
                return false;
            }
        }
        true
    }

    pub fn cancel(&self) -> bool {
        self.cancellation_token.cancel();
        true
    }
}

#[derive(Debug, Clone)]
pub struct ForwardedDecision {
    pub thread_id: String,
}

#[derive(Default)]
pub(super) struct ActiveThreadRunRegistry {
    handles: RwLock<HashMap<String, ThreadRunHandle>>,
    run_index: RwLock<HashMap<String, String>>,
}

impl ActiveThreadRunRegistry {
    async fn register(
        &self,
        run_id: String,
        agent_id: &str,
        thread_id: &str,
        token: RunCancellationToken,
    ) {
        let mut run_index = self.run_index.write().await;
        let mut handles = self.handles.write().await;
        if let Some(old) = handles.get(thread_id) {
            run_index.remove(&old.run_id);
        }
        run_index.insert(run_id.clone(), thread_id.to_string());
        handles.insert(
            thread_id.to_string(),
            ThreadRunHandle {
                thread_id: thread_id.to_string(),
                run_id,
                agent_id: agent_id.to_string(),
                cancellation_token: token,
                decision_tx: Arc::new(RwLock::new(None)),
                pending_decisions: Arc::new(Mutex::new(Vec::new())),
                stream_fanout: Arc::new(RwLock::new(None)),
            },
        );
    }

    async fn handle_by_thread(&self, thread_id: &str) -> Option<ThreadRunHandle> {
        let handles = self.handles.read().await;
        let handle = handles.get(thread_id)?;
        Some(handle.clone())
    }

    async fn handle_by_run_id(&self, run_id: &str) -> Option<ThreadRunHandle> {
        let run_index = self.run_index.read().await;
        let thread_id = run_index.get(run_id)?;
        let handles = self.handles.read().await;
        let handle = handles.get(thread_id)?;
        Some(handle.clone())
    }

    pub(super) async fn remove_by_run_id(&self, run_id: &str) {
        if let Some(thread_id) = self.run_index.write().await.remove(run_id) {
            self.handles.write().await.remove(&thread_id);
        }
    }
}

impl AgentOs {
    pub(crate) async fn register_thread_run_handle(
        &self,
        run_id: String,
        agent_id: &str,
        thread_id: &str,
        token: RunCancellationToken,
    ) {
        self.active_runs
            .register(run_id, agent_id, thread_id, token)
            .await;
    }

    pub(crate) async fn bind_thread_run_decision_tx(
        &self,
        run_id: &str,
        tx: mpsc::UnboundedSender<ToolCallDecision>,
    ) -> bool {
        let Some(handle) = self.active_runs.handle_by_run_id(run_id).await else {
            return false;
        };
        handle.bind_decision_tx(tx).await
    }

    pub(crate) async fn remove_thread_run_handle(&self, run_id: &str) {
        self.active_runs.remove_by_run_id(run_id).await;
    }

    pub async fn bind_thread_run_stream_fanout(
        &self,
        run_id: &str,
        fanout: broadcast::Sender<Bytes>,
    ) -> bool {
        let Some(handle) = self.active_runs.handle_by_run_id(run_id).await else {
            return false;
        };
        handle.bind_stream_fanout(fanout).await;
        true
    }

    pub async fn subscribe_thread_run_stream(
        &self,
        run_id: &str,
    ) -> Option<broadcast::Receiver<Bytes>> {
        let handle = self.active_runs.handle_by_run_id(run_id).await?;
        handle.subscribe_stream_fanout().await
    }

    pub(crate) async fn active_thread_run_by_run_id(
        &self,
        run_id: &str,
    ) -> Option<ThreadRunHandle> {
        self.active_runs.handle_by_run_id(run_id).await
    }

    pub async fn active_run_id_for_thread(
        &self,
        agent_id: &str,
        thread_id: &str,
    ) -> Option<String> {
        let handle = self.active_runs.handle_by_thread(thread_id).await?;
        if !handle.can_own(agent_id) {
            return None;
        }
        Some(handle.run_id().to_string())
    }

    pub async fn forward_decisions_by_thread(
        &self,
        agent_id: &str,
        thread_id: &str,
        decisions: &[ToolCallDecision],
    ) -> Option<ForwardedDecision> {
        let handle = self.active_runs.handle_by_thread(thread_id).await?;
        if !handle.can_own(agent_id) {
            return None;
        }
        if handle.send_decisions(decisions).await {
            Some(ForwardedDecision {
                thread_id: handle.thread_id().to_string(),
            })
        } else {
            self.active_runs.remove_by_run_id(handle.run_id()).await;
            None
        }
    }

    pub async fn forward_decisions_by_run_id(
        &self,
        run_id: &str,
        decisions: &[ToolCallDecision],
    ) -> Option<ForwardedDecision> {
        let handle = self.active_runs.handle_by_run_id(run_id).await?;
        if handle.send_decisions(decisions).await {
            Some(ForwardedDecision {
                thread_id: handle.thread_id().to_string(),
            })
        } else {
            self.active_runs.remove_by_run_id(run_id).await;
            None
        }
    }

    pub async fn cancel_active_run_by_id(&self, run_id: &str) -> bool {
        let Some(handle) = self.active_runs.handle_by_run_id(run_id).await else {
            return false;
        };
        if !handle.cancel() {
            self.active_runs.remove_by_run_id(run_id).await;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_registry() -> ActiveThreadRunRegistry {
        ActiveThreadRunRegistry::default()
    }

    fn new_channel() -> (
        mpsc::UnboundedSender<ToolCallDecision>,
        mpsc::UnboundedReceiver<ToolCallDecision>,
    ) {
        mpsc::unbounded_channel()
    }

    #[tokio::test]
    async fn registry_register_and_lookup_by_thread() {
        let reg = new_registry();
        let (_decision_tx, _decision_rx) = new_channel();
        let token = RunCancellationToken::new();
        reg.register("run-1".into(), "agent-a", "thread-1", token)
            .await;

        let handle = reg
            .handle_by_thread("thread-1")
            .await
            .expect("handle should exist");
        assert_eq!(handle.run_id(), "run-1");
        assert!(handle.can_own("agent-a"));
    }

    #[tokio::test]
    async fn registry_lookup_by_run_id() {
        let reg = new_registry();
        let (_decision_tx, _decision_rx) = new_channel();
        let token = RunCancellationToken::new();
        reg.register("run-1".into(), "agent-a", "thread-1", token.clone())
            .await;

        let retrieved = reg
            .handle_by_run_id("run-1")
            .await
            .expect("handle should exist");
        let retrieved_token = retrieved.cancellation_token();
        retrieved_token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn registry_thread_key_enforces_single_run_per_thread() {
        let reg = new_registry();
        let (_decision_tx_a, _decision_rx_a) = new_channel();
        let (_decision_tx_b, _decision_rx_b) = new_channel();
        let token_a = RunCancellationToken::new();
        let token_b = RunCancellationToken::new();
        reg.register("run-a".into(), "agent-a", "shared-thread", token_a)
            .await;
        reg.register("run-b".into(), "agent-b", "shared-thread", token_b)
            .await;

        let handle = reg
            .handle_by_thread("shared-thread")
            .await
            .expect("handle should exist");
        assert_eq!(handle.run_id(), "run-b");
        assert!(handle.can_own("agent-b"));
        assert!(reg.handle_by_run_id("run-a").await.is_none());
        assert!(reg.handle_by_run_id("run-b").await.is_some());
    }

    #[tokio::test]
    async fn registry_remove_cleans_both_indexes() {
        let reg = new_registry();
        let (_decision_tx, _decision_rx) = new_channel();
        let token = RunCancellationToken::new();
        reg.register("run-1".into(), "agent-a", "thread-1", token)
            .await;

        reg.remove_by_run_id("run-1").await;
        assert!(reg.handle_by_thread("thread-1").await.is_none());
        assert!(reg.handle_by_run_id("run-1").await.is_none());
    }

    #[tokio::test]
    async fn stream_fanout_bind_and_subscribe() {
        let reg = new_registry();
        let token = RunCancellationToken::new();
        reg.register("run-1".into(), "agent-a", "thread-1", token)
            .await;
        let handle = reg
            .handle_by_run_id("run-1")
            .await
            .expect("handle should exist");
        let (fanout, _rx) = broadcast::channel::<Bytes>(8);
        handle.bind_stream_fanout(fanout.clone()).await;
        let mut sub = handle
            .subscribe_stream_fanout()
            .await
            .expect("subscription should exist");
        fanout
            .send(Bytes::from_static(b"chunk"))
            .expect("send should work");
        let got = sub.recv().await.expect("subscriber should receive chunk");
        assert_eq!(got, Bytes::from_static(b"chunk"));
    }
}
