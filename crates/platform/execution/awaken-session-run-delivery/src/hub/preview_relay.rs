// @formal surface requirement=protocol.ui_adapters.stream_projection
// @formal reason: A bounded observation transport across trusted Coordinators. Dispatch and committed state retain authority.
use super::{ThreadEvent, ThreadEventHub};
use awaken_agent_contract::stream::event::Observation;
use awaken_lease_contract::fence::Epoch;
use awaken_session_execution_contract::ThreadLiveObservation;
use awaken_sync::LeafLock;
use futures_lite::StreamExt as _;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

const BACKLOG: usize = 128;
const MAX_IDENTITIES: usize = 4096;
const MAX_BYTES: usize = 96 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Packet {
    source: Uuid,
    epoch: Epoch,
    stream: Uuid,
    sequence: u64,
    observation: Observation,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Response {
    thread: String,
    run: String,
    step: usize,
    response: usize,
    epoch: Epoch,
}

impl Packet {
    fn response(&self) -> Option<Response> {
        let coordinate = self.observation.assistant_response.as_ref()?;
        Some(Response {
            thread: coordinate.thread_id.0.clone(),
            run: self.observation.event.run_id.0.clone(),
            step: coordinate.step,
            response: coordinate.response,
            epoch: self.epoch,
        })
    }
}

pub(super) struct Relay {
    source: Uuid,
    outgoing: mpsc::Sender<Packet>,
    sequences: LeafLock<HashMap<Response, (Uuid, u64)>>,
    sending: JoinHandle<()>,
    receiving: JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.sending.abort();
        self.receiving.abort();
    }
}

impl Relay {
    pub(super) fn offer(&self, thread_key: &str, live: &ThreadLiveObservation) -> Option<Packet> {
        let mut packet = Packet {
            source: self.source,
            epoch: live.epoch?,
            stream: Uuid::nil(),
            sequence: 0,
            observation: live.observation.clone(),
        };
        let response = packet.response()?;
        if response.thread != thread_key {
            return None;
        }
        let mut sequences = self.sequences.lock();
        if sequences.len() >= MAX_IDENTITIES && !sequences.contains_key(&response) {
            // Reuse publisher memory. A peer with an older prefix ignores a
            // reset sequence and will reconcile at the durable final message.
            sequences.clear();
        }
        let (stream, sequence) = sequences
            .entry(response)
            .or_insert_with(|| (Uuid::new_v4(), 0));
        packet.stream = *stream;
        packet.sequence = *sequence;
        *sequence = sequence.checked_add(1)?;
        let _ = self.outgoing.try_send(packet.clone());
        Some(packet)
    }
}

#[derive(Default)]
pub(super) struct OrderedPreviews(HashMap<Response, (Uuid, Option<u64>)>);

impl OrderedPreviews {
    fn accept(&mut self, response: Response, packet: &Packet) -> bool {
        if self.0.len() >= MAX_IDENTITIES && !self.0.contains_key(&response) {
            return false;
        }
        let (source, next) = self.0.entry(response).or_insert((packet.stream, Some(0)));
        let Some(expected) = *next else { return false };
        if *source != packet.stream || packet.sequence > expected {
            // A missing prefix or a different Coordinator's partial response
            // cannot safely extend the preview already rendered here.
            *next = None;
            return false;
        }
        if packet.sequence != expected {
            return false;
        }
        *next = expected.checked_add(1);
        true
    }
}

impl ThreadEventHub {
    /// Route claim-attributed previews across a trusted deployment's private
    /// NATS subject. The local Hub and committed replay remain available if a
    /// connected broker subsequently disconnects.
    pub async fn attach_nats_preview(
        self: &Arc<Self>,
        url: &str,
        subject: &str,
    ) -> Result<(), String> {
        if subject.is_empty()
            || subject.split('.').any(|part| part.is_empty())
            || subject
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'*' | b'>'))
        {
            return Err("preview relay requires a literal NATS subject".into());
        }
        let source = Uuid::new_v4();
        let (client, mut messages) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let client = async_nats::connect(url)
                    .await
                    .map_err(|_| "preview relay could not connect to NATS")?;
                let mut messages = client
                    .subscribe(subject.to_owned())
                    .await
                    .map_err(|_| "preview relay could not subscribe to NATS")?;
                // Receiving our own probe confirms the broker processed the
                // subscription before readiness. Other listeners ignore it.
                let probe = format!("preview-probe:{source}");
                client
                    .publish(subject.to_owned(), probe.clone().into())
                    .await
                    .map_err(|_| "preview relay readiness publication failed")?;
                while let Some(message) = messages.next().await {
                    if message.payload.as_ref() == probe.as_bytes() {
                        return Ok((client, messages));
                    }
                }
                Err("preview relay subscription closed before readiness")
            })
            .await
            .map_err(|_| "preview relay readiness timed out")??;
        let weak = Arc::downgrade(self);
        let receiving = tokio::spawn(async move {
            while let Some(message) = messages.next().await {
                let Some(hub) = weak.upgrade() else { break };
                if message.payload.len() <= MAX_BYTES
                    && let Ok(packet) = serde_json::from_slice::<Packet>(&message.payload)
                    && packet.source != source
                {
                    hub.receive_preview(packet);
                }
            }
        });
        let (outgoing, mut pending) = mpsc::channel::<Packet>(BACKLOG);
        let subject = subject.to_owned();
        let sending = tokio::spawn(async move {
            while let Some(packet) = pending.recv().await {
                if let Ok(body) = serde_json::to_vec(&packet)
                    && body.len() <= MAX_BYTES
                    && client.connection_state() == async_nats::connection::State::Connected
                {
                    let _ = client.publish(subject.clone(), body.into()).await;
                }
            }
        });
        self.relay
            .set(Relay {
                source,
                outgoing,
                sequences: Default::default(),
                sending,
                receiving,
            })
            .map_err(|_| "preview relay already attached".to_owned())
    }

    pub(super) fn receive_preview(&self, packet: Packet) {
        let Some(response) = packet.response() else {
            return;
        };
        let epoch = packet.epoch;
        if !awaken_agent_contract::event::classify(&packet.observation.event.kind).live {
            return;
        }
        // Keep order admission and channel publication in the same critical
        // section so a concurrent local or relayed frame cannot overtake it.
        if let Ok(channels) = self.channels.lock()
            && let Some(channel) = channels.get(&response.thread)
            && channel.previews.lock().accept(response, &packet)
        {
            channel.send(ThreadEvent::ClaimedLive(ThreadLiveObservation {
                observation: packet.observation,
                epoch: Some(epoch),
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::{
        agent::{run::Id as RunId, thread::Id as ThreadId},
        event::{AgentEvent, Delta},
    };
    use std::time::Duration;

    fn text(thread: &str, delta: &str) -> Observation {
        Observation::assistant_delta(
            RunId("preview-run".into()),
            ThreadId(thread.into()),
            0,
            0,
            AgentEvent::Delta(Delta::TextDelta {
                delta: delta.into(),
            }),
        )
    }

    #[tokio::test]
    async fn relayed_previews_reject_duplicates_gaps_and_predecessors() {
        let hub = ThreadEventHub::new();
        let mut subscription = hub.live_subscription("thread").unwrap();
        let mut other = hub.subscribe("other").unwrap();
        let source = Uuid::new_v4();
        let stream = Uuid::new_v4();
        let packet = |epoch, sequence, delta| Packet {
            source,
            stream,
            epoch: Epoch::from_persisted(epoch).unwrap(),
            sequence,
            observation: text("thread", delta),
        };
        let first = packet(3, 0, "start");
        hub.receive_preview(first.clone());
        hub.receive_preview(first);
        hub.receive_preview(packet(3, 2, "after gap"));
        hub.receive_preview(packet(3, 1, "late fill"));
        let live = subscription.recv().await.unwrap().unwrap();
        assert_eq!(live.observation, text("thread", "start"));

        hub.receive_preview(packet(4, 0, "replacement"));
        hub.receive_preview(packet(3, 3, "predecessor"));
        let live = subscription.recv().await.unwrap().unwrap();
        assert_eq!(live.observation, text("thread", "replacement"));
        assert_eq!(live.epoch, Epoch::from_persisted(4));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), subscription.recv())
                .await
                .is_err()
        );
        assert!(matches!(
            other.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    #[ignore = "requires an isolated Core NATS broker and AWAKEN_TEST_PREVIEW_NATS_URL"]
    async fn core_nats_relay_carries_previews_between_independent_hubs() {
        let broker = std::env::var("AWAKEN_TEST_PREVIEW_NATS_URL").expect("real Core NATS fixture");
        let subject = format!("awaken.preview.tests.{}", Uuid::new_v4());
        let a = Arc::new(ThreadEventHub::new());
        let b = Arc::new(ThreadEventHub::new());
        let c = Arc::new(ThreadEventHub::new());
        a.attach_nats_preview(&broker, &subject).await.unwrap();
        b.attach_nats_preview(&broker, &subject).await.unwrap();
        c.attach_nats_preview(&broker, &format!("{subject}.isolated"))
            .await
            .unwrap();
        let mut local = a.live_subscription("thread").unwrap();
        let mut remote = b.live_subscription("thread").unwrap();
        let mut isolated = c.subscribe("thread").unwrap();
        let claim = awaken_worker_contract::RunClaim {
            run_id: RunId("preview-run".into()),
            owner: "worker".into(),
            epoch: 7,
        };
        awaken_dispatch_contract::with_preview_claim_epoch(&claim, async {
            for fragment in ["one", "two", "three"] {
                a.publish("thread", ThreadEvent::Live(text("thread", fragment)));
            }
        })
        .await;
        for fragment in ["one", "two", "three"] {
            for subscription in [&mut local, &mut remote] {
                let live = tokio::time::timeout(Duration::from_secs(5), subscription.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(live.observation, text("thread", fragment));
                assert_eq!(live.epoch, Epoch::from_persisted(7));
            }
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), local.recv())
                .await
                .is_err()
        );
        assert!(matches!(
            isolated.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
}
