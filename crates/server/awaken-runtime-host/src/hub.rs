//! `ThreadEventHub` — the per-thread live observation point shared by protocol
//! adapters.
//!
//! A turn is driven by exactly one protocol at a time (the *submitter*), but more
//! than one protocol may be bound to the same thread — e.g. an AI SDK frontend
//! drives the run while a Managed-Agents backend services a client-executed tool.
//! The non-submitting adapter needs to *observe* the same thread's committed
//! deltas without issuing its own (competing) turn. This hub is that shared point:
//! the submitter publishes each committed delta by scoped thread id, and any
//! number of observers subscribe.
//!
//! The runtime here is turn-synchronous, so the hub carries committed-message
//! deltas rather than token-level events; the port is identical to the streaming
//! shape and can carry richer events unchanged once a live sink is plumbed.

use std::collections::HashMap;
use std::sync::Mutex;

use awaken_agent_contract::agent::message::Message;
use tokio::sync::broadcast;

/// Backlog per thread. Sized so a late-attaching observer still sees a short
/// turn's opening delta; an observer that lags past this loses events (the turn
/// still completes for the submitter).
const HUB_CAPACITY: usize = 256;

/// One neutral observation on a thread. Neutral by construction: no protocol
/// vocabulary — each adapter projects it into its own wire shape.
#[derive(Debug, Clone)]
pub enum ThreadEvent {
    /// Messages committed during one step (a turn or a resume).
    Committed(Vec<Message>),
    /// The step reached a terminal position. `waiting` is true when the run
    /// parked (awaiting a tool decision / client result), false on natural end.
    StepEnded { waiting: bool },
    /// An external ACP agent's bring-up progressed (installing an npx adapter,
    /// launching the process, initializing the handshake, ready, or failed). A UI
    /// renders this as a "starting agent…" affordance while a dynamic install runs.
    /// `stage` is the snake_case [`awaken_run_executor_acp::AcpLaunchStage`]; `detail`
    /// is optional human-facing context.
    AgentLaunch {
        stage: String,
        detail: Option<String>,
    },
}

/// Shared registry of per-thread broadcast channels.
#[derive(Default)]
pub struct ThreadEventHub {
    channels: Mutex<HashMap<String, broadcast::Sender<ThreadEvent>>>,
}

impl ThreadEventHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn sender(&self, thread_key: &str) -> broadcast::Sender<ThreadEvent> {
        self.channels
            .lock()
            .expect("hub channel lock")
            .entry(thread_key.to_string())
            .or_insert_with(|| broadcast::channel(HUB_CAPACITY).0)
            .clone()
    }

    /// Subscribe to a thread's live deltas. Only events published after this call
    /// are delivered, so an observer must subscribe before the turn it wants to
    /// watch is submitted.
    pub fn subscribe(&self, thread_key: &str) -> broadcast::Receiver<ThreadEvent> {
        self.sender(thread_key).subscribe()
    }

    /// Publish one event to a thread's observers (a no-op if none are attached).
    pub fn publish(&self, thread_key: &str, event: ThreadEvent) {
        let _ = self.sender(thread_key).send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id, Role};

    fn msg(text: &str) -> Message {
        Message::text(Id(text.to_string()), Role::Assistant, text)
    }

    #[tokio::test]
    async fn observer_sees_published_delta() {
        let hub = ThreadEventHub::new();
        let mut observer = hub.subscribe("t1");
        hub.publish("t1", ThreadEvent::Committed(vec![msg("hi")]));
        match observer.recv().await.unwrap() {
            ThreadEvent::Committed(m) => assert_eq!(m.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_is_per_thread() {
        let hub = ThreadEventHub::new();
        let mut a = hub.subscribe("a");
        let mut b = hub.subscribe("b");
        hub.publish("a", ThreadEvent::StepEnded { waiting: false });
        assert!(a.try_recv().is_ok());
        assert!(b.try_recv().is_err());
    }

    #[tokio::test]
    async fn every_observer_on_a_thread_sees_the_delta() {
        // The whole point of the hub: a non-submitting adapter observes the same
        // thread's committed deltas. Two observers on one thread must BOTH receive.
        let hub = ThreadEventHub::new();
        let mut o1 = hub.subscribe("t1");
        let mut o2 = hub.subscribe("t1");
        hub.publish("t1", ThreadEvent::Committed(vec![msg("hi")]));
        assert!(matches!(o1.try_recv(), Ok(ThreadEvent::Committed(_))));
        assert!(matches!(o2.try_recv(), Ok(ThreadEvent::Committed(_))));
    }

    #[tokio::test]
    async fn a_late_subscriber_misses_a_pre_subscription_publish() {
        // The documented ordering guarantee: only events published AFTER subscribe are
        // delivered, so an observer must attach before the turn it wants to watch. A
        // publish before the (first ever) subscribe is not replayed to the newcomer.
        let hub = ThreadEventHub::new();
        hub.publish("t1", ThreadEvent::StepEnded { waiting: false });
        let mut late = hub.subscribe("t1");
        assert!(
            late.try_recv().is_err(),
            "a subscriber attached after the publish sees nothing buffered for it"
        );
        // But it does see the NEXT event, proving the channel is live (not the wrong one).
        hub.publish("t1", ThreadEvent::StepEnded { waiting: true });
        assert!(matches!(
            late.try_recv(),
            Ok(ThreadEvent::StepEnded { waiting: true })
        ));
    }

    #[tokio::test]
    async fn publish_with_no_observers_is_a_silent_no_op() {
        // A submitter always publishes; a thread nobody observes must not error/panic
        // (the send result is deliberately swallowed).
        let hub = ThreadEventHub::new();
        hub.publish("unobserved", ThreadEvent::StepEnded { waiting: false });
    }
}
