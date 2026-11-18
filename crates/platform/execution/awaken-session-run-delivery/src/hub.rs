// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This source implements the repository-controlled session product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! `ThreadEventHub` — the per-thread live observation point shared by protocol
//! adapters.
//!
//! A Run is driven by exactly one protocol at a time (the *submitter*), but more
//! than one protocol may be bound to the same thread — e.g. an AI SDK frontend
//! drives the run while a Managed-Agents backend services a client-executed tool.
//! The non-submitting adapter needs to *observe* the same thread's committed
//! deltas without issuing its own competing Run. This hub is that shared point:
//! the submitter publishes each committed delta by scoped thread id, and any
//! number of observers subscribe.
//!
//! The same channel carries best-effort neutral token deltas and committed-message
//! observations. Neither is a second truth store: the former is lossy preview,
//! while the latter is reconstructed from the canonical commit.

use std::{collections::HashMap, sync::Arc};

use crate::DeliveryLock;
use awaken_sync::InvariantLock;

use awaken_agent_contract::agent::message::Message;
use tokio::sync::broadcast;

#[cfg(feature = "nats")]
mod preview_relay;

/// Backlog per active Thread. A receiver that lags past this loses volatile events.
const HUB_CAPACITY: usize = 256;

/// One neutral observation on a thread. Neutral by construction: no protocol
/// vocabulary — each adapter projects it into its own wire shape.
#[derive(Debug, Clone)]
pub enum ThreadEvent {
    /// Best-effort model progress for this exact logical Thread. It uses the
    /// Runtime's one neutral stream vocabulary; protocol adapters decide whether
    /// and how to render it. Never durable truth.
    Live(awaken_agent_contract::stream::event::Observation),
    /// The same observation, attributed by the verified Worker transport.
    ClaimedLive(awaken_session_execution_contract::ThreadLiveObservation),
    /// Messages committed during one Step (a new Run or a resume).
    Committed(Vec<Message>),
    /// The step reached a terminal position. `awaiting` is true when the run
    /// awaits a tool decision / client result, false on natural end.
    StepEnded { awaiting: bool },
    /// An external ACP agent's bring-up progressed (launching the process,
    /// initializing the handshake, ready, or failed). A UI renders this as a
    /// "starting agent…" affordance.
    /// `stage` is the snake_case [`awaken_acp_contract::AcpLaunchStage`]; `detail`
    /// is optional human-facing context.
    AgentLaunch {
        stage: String,
        detail: Option<String>,
    },
}

/// Shared registry of per-thread broadcast channels.
#[derive(Default)]
pub struct ThreadEventHub {
    channels: Arc<Channels>,
    #[cfg(feature = "nats")]
    relay: std::sync::OnceLock<preview_relay::Relay>,
}

type Channels = InvariantLock<HashMap<String, Channel>>;
type PreviewClaim = awaken_sync::LeafLock<Option<(String, u64)>>;

struct Channel {
    sender: broadcast::Sender<ThreadEvent>,
    claim: Arc<PreviewClaim>,
    #[cfg(feature = "nats")]
    previews: awaken_sync::LeafLock<preview_relay::OrderedPreviews>,
}

impl Channel {
    fn observe_preview_epoch(&self, run: &str, epoch: awaken_lease_contract::fence::Epoch) -> bool {
        let mut claim = self.claim.lock();
        if claim
            .as_ref()
            .is_some_and(|(current, latest)| current == run && epoch.get() < *latest)
        {
            return false;
        }
        *claim = Some((run.to_owned(), epoch.get()));
        true
    }

    fn send(&self, event: ThreadEvent) {
        if let ThreadEvent::ClaimedLive(live) = &event
            && let Some(epoch) = live.epoch
            && !self.observe_preview_epoch(&live.observation.event.run_id.0, epoch)
        {
            return;
        }
        let _ = self.sender.send(event);
    }
}

/// Owns a receiver and removes its channel when the last observer leaves.
pub struct ThreadSubscription {
    channels: Arc<Channels>,
    key: String,
    receiver: Option<broadcast::Receiver<ThreadEvent>>,
    claim: Arc<PreviewClaim>,
}

impl ThreadSubscription {
    pub async fn recv(&mut self) -> Result<ThreadEvent, broadcast::error::RecvError> {
        self.receiver
            .as_mut()
            .expect("live subscription")
            .recv()
            .await
    }

    pub fn try_recv(&mut self) -> Result<ThreadEvent, broadcast::error::TryRecvError> {
        self.receiver
            .as_mut()
            .expect("live subscription")
            .try_recv()
    }
}

impl Drop for ThreadSubscription {
    fn drop(&mut self) {
        if let Ok(mut channels) = self.channels.lock() {
            // Serialize deregistration with subscribe, so dropping the last
            // previous receiver cannot remove a newly attached receiver.
            drop(self.receiver.take());
            if channels
                .get(&self.key)
                .is_some_and(|channel| channel.sender.receiver_count() == 0)
            {
                channels.remove(&self.key);
            }
        }
    }
}

impl ThreadEventHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to a thread's live deltas. Only events published after this call
    /// are delivered, so an observer must subscribe before the Run it wants to
    /// watch is submitted.
    pub fn subscribe(&self, thread_key: &str) -> Result<ThreadSubscription, DeliveryLock> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| DeliveryLock::ThreadEventHub)?;
        let channel = channels
            .entry(thread_key.to_owned())
            .or_insert_with(|| Channel {
                sender: broadcast::channel(HUB_CAPACITY).0,
                claim: Arc::default(),
                #[cfg(feature = "nats")]
                previews: Default::default(),
            });
        let receiver = channel.sender.subscribe();
        let claim = channel.claim.clone();
        Ok(ThreadSubscription {
            channels: self.channels.clone(),
            key: thread_key.to_owned(),
            receiver: Some(receiver),
            claim,
        })
    }

    /// Publish a live observation locally and, when configured, to peers.
    pub fn publish(&self, thread_key: &str, event: ThreadEvent) {
        let event = match (event, awaken_dispatch_contract::preview_claim_epoch()) {
            (ThreadEvent::Live(observation), Some(epoch)) => {
                ThreadEvent::ClaimedLive(awaken_session_execution_contract::ThreadLiveObservation {
                    observation,
                    epoch: Some(epoch),
                })
            }
            (event, _) => event,
        };
        #[cfg(feature = "nats")]
        if let (Some(relay), ThreadEvent::ClaimedLive(live)) = (self.relay.get(), &event)
            && let Some(packet) = relay.offer(thread_key, live)
        {
            self.receive_preview(packet);
            return;
        }
        self.publish_local(thread_key, event);
    }

    fn publish_local(&self, thread_key: &str, event: ThreadEvent) {
        match self.channels.lock() {
            Ok(channels) => {
                if let Some(channel) = channels.get(thread_key) {
                    channel.send(event);
                }
            }
            Err(_) => tracing::error!(
                code = crate::LOCK_POISONED_CODE,
                "live Thread event dropped"
            ),
        }
    }

    /// Open the neutral Session-contract view over this same channel. This is a
    /// wrapper, not a second fan-out path: committed/launch observations are
    /// skipped and only `ThreadEvent::Live` crosses the port.
    pub fn live_subscription(
        &self,
        thread_key: &str,
    ) -> Result<
        Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>,
        DeliveryLock,
    > {
        Ok(Box::new(HubLiveSubscription {
            receiver: self.subscribe(thread_key)?,
        }))
    }
}

struct HubLiveSubscription {
    receiver: ThreadSubscription,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionThreadLiveSubscription for HubLiveSubscription {
    async fn recv(
        &mut self,
    ) -> Result<
        Option<awaken_session_execution_contract::ThreadLiveObservation>,
        awaken_runtime_contract::runs::RunError,
    > {
        use awaken_session_execution_contract::ThreadLiveObservation;
        loop {
            match self.receiver.recv().await {
                Ok(ThreadEvent::Live(observation)) => {
                    if self
                        .receiver
                        .claim
                        .lock()
                        .as_ref()
                        .is_some_and(|(run, _)| run == &observation.event.run_id.0)
                    {
                        continue;
                    }
                    return Ok(Some(ThreadLiveObservation {
                        observation,
                        epoch: None,
                    }));
                }
                Ok(ThreadEvent::ClaimedLive(live)) => {
                    // Recheck attribution at dequeue: a newer publication can
                    // fence an older queued preview before this receiver sees it.
                    if live.epoch.is_some_and(|epoch| {
                        self.receiver
                            .claim
                            .lock()
                            .as_ref()
                            .is_some_and(|(run, newest)| {
                                run == &live.observation.event.run_id.0 && epoch.get() < *newest
                            })
                    }) {
                        continue;
                    }
                    return Ok(Some(live));
                }
                Ok(
                    ThreadEvent::Committed(_)
                    | ThreadEvent::StepEnded { .. }
                    | ThreadEvent::AgentLaunch { .. },
                ) => continue,
                // Preview is best-effort: lag drops only volatile prefixes.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id, Role};

    fn msg(text: &str) -> Message {
        Message::text(Id(text.to_string()), Role::Assistant, text)
    }

    #[test]
    fn a_poisoned_channel_map_refuses_subscribers_and_drops_publications() {
        // Row H2: the Hub refuses reads and drops live publications.
        let hub = ThreadEventHub::new();
        crate::poison_for_test(&hub.channels);
        assert!(
            matches!(hub.subscribe("t1"), Err(DeliveryLock::ThreadEventHub)),
            "H2 subscribe"
        );
        assert!(
            matches!(
                hub.live_subscription("t1"),
                Err(DeliveryLock::ThreadEventHub)
            ),
            "H2 live subscription"
        );
        hub.publish("t1", ThreadEvent::StepEnded { awaiting: false });
    }

    #[tokio::test]
    async fn observer_sees_published_delta() {
        let hub = ThreadEventHub::new();
        let mut observer = hub.subscribe("t1").expect("healthy hub");
        hub.publish("t1", ThreadEvent::Committed(vec![msg("hi")]));
        match tokio::time::timeout(std::time::Duration::from_secs(30), observer.recv())
            .await
            .expect("timed out waiting for observer.recv()")
            .unwrap()
        {
            ThreadEvent::Committed(m) => assert_eq!(m.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_is_per_thread() {
        let hub = ThreadEventHub::new();
        let mut a = hub.subscribe("a").expect("healthy hub");
        let mut b = hub.subscribe("b").expect("healthy hub");
        hub.publish("a", ThreadEvent::StepEnded { awaiting: false });
        assert!(a.try_recv().is_ok());
        assert!(b.try_recv().is_err());
    }

    #[tokio::test]
    async fn every_observer_on_a_thread_sees_the_delta() {
        // The whole point of the hub: a non-submitting adapter observes the same
        // thread's committed deltas. Two observers on one thread must BOTH receive.
        let hub = ThreadEventHub::new();
        let mut o1 = hub.subscribe("t1").expect("healthy hub");
        let mut o2 = hub.subscribe("t1").expect("healthy hub");
        hub.publish("t1", ThreadEvent::Committed(vec![msg("hi")]));
        assert!(matches!(o1.try_recv(), Ok(ThreadEvent::Committed(_))));
        assert!(matches!(o2.try_recv(), Ok(ThreadEvent::Committed(_))));
    }

    #[tokio::test]
    async fn a_late_subscriber_misses_a_pre_subscription_publish() {
        // The documented ordering guarantee: only events published AFTER subscribe are
        // delivered, so an observer must attach before the Run it wants to watch. A
        // publish before the (first ever) subscribe is not replayed to the newcomer.
        let hub = ThreadEventHub::new();
        hub.publish("t1", ThreadEvent::StepEnded { awaiting: false });
        let mut late = hub.subscribe("t1").expect("healthy hub");
        assert!(
            late.try_recv().is_err(),
            "a subscriber attached after the publish sees nothing buffered for it"
        );
        // But it does see the NEXT event, proving the channel is live (not the wrong one).
        hub.publish("t1", ThreadEvent::StepEnded { awaiting: true });
        assert!(matches!(
            late.try_recv(),
            Ok(ThreadEvent::StepEnded { awaiting: true })
        ));
    }

    #[tokio::test]
    async fn publish_with_no_observers_is_a_silent_no_op() {
        let hub = ThreadEventHub::new();
        hub.publish("unobserved", ThreadEvent::StepEnded { awaiting: false });
        assert!(
            hub.channels
                .lock()
                .is_ok_and(|channels| channels.is_empty())
        );
    }

    #[test]
    fn only_the_last_subscriber_releases_the_thread_channel() {
        let hub = ThreadEventHub::new();
        let first = hub.subscribe("t1").unwrap();
        let mut second = hub.subscribe("t1").unwrap();
        drop(first);
        hub.publish("t1", ThreadEvent::StepEnded { awaiting: true });
        assert!(matches!(
            second.try_recv(),
            Ok(ThreadEvent::StepEnded { awaiting: true })
        ));
        drop(second);
        assert!(
            hub.channels
                .lock()
                .is_ok_and(|channels| channels.is_empty())
        );

        let mut replacement = hub.subscribe("t1").unwrap();
        hub.publish("t1", ThreadEvent::StepEnded { awaiting: false });
        assert!(matches!(
            replacement.try_recv(),
            Ok(ThreadEvent::StepEnded { awaiting: false })
        ));
    }

    #[tokio::test]
    async fn queued_preview_retains_its_claim_epoch_after_publication_returns() {
        use awaken_agent_contract::agent::run::Id as RunId;
        use awaken_agent_contract::event::{AgentEvent, Delta};
        use awaken_agent_contract::stream::event::Event;

        let hub = ThreadEventHub::new();
        let mut subscription = hub.live_subscription("t").unwrap();
        let observation = Event {
            run_id: RunId("run".into()),
            kind: AgentEvent::Delta(Delta::TextDelta {
                delta: "live".into(),
            }),
        }
        .into();
        let claim = awaken_worker_contract::RunClaim {
            run_id: RunId("run".into()),
            owner: "worker".into(),
            epoch: 7,
        };
        awaken_dispatch_contract::with_preview_claim_epoch(&claim, async {
            hub.publish("t", ThreadEvent::Live(observation));
        })
        .await;
        assert_eq!(awaken_dispatch_contract::preview_claim_epoch(), None);
        let live = tokio::time::timeout(std::time::Duration::from_secs(30), subscription.recv())
            .await
            .expect("the queued observation reaches the subscriber")
            .unwrap()
            .unwrap();
        assert_eq!(live.observation.event.run_id.0, "run");
        assert_eq!(live.epoch.map(|epoch| epoch.get()), Some(7));
    }

    #[tokio::test]
    async fn a_new_publication_fences_older_queued_previews_before_the_receiver_observes_it() {
        use awaken_agent_contract::agent::run::Id as RunId;
        use awaken_agent_contract::event::{AgentEvent, Delta};
        use awaken_agent_contract::stream::event::Event;
        let hub = ThreadEventHub::new();
        let mut receiver = hub.live_subscription("t").unwrap();
        for epoch in [7, 8] {
            let claim = awaken_worker_contract::RunClaim {
                run_id: RunId("run".into()),
                owner: "worker".into(),
                epoch,
            };
            awaken_dispatch_contract::with_preview_claim_epoch(&claim, async {
                hub.publish(
                    "t",
                    ThreadEvent::Live(
                        Event {
                            run_id: RunId("run".into()),
                            kind: AgentEvent::Delta(Delta::TextDelta {
                                delta: epoch.to_string(),
                            }),
                        }
                        .into(),
                    ),
                );
            })
            .await;
        }
        let live = tokio::time::timeout(std::time::Duration::from_secs(30), receiver.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(live.epoch.map(|epoch| epoch.get()), Some(8));
        assert!(
            hub.channels
                .lock()
                .is_ok_and(|channels| channels.len() == 1)
        );
    }
}
