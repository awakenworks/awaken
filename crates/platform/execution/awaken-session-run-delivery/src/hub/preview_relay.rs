// @formal surface requirement=protocol.ui_adapters.stream_projection
// @formal reason: A bounded observation transport across trusted Coordinators. Dispatch and committed state retain authority.
use super::{ThreadEvent, ThreadEventHub};
use awaken_agent_contract::stream::event::Observation;
use awaken_lease_contract::fence::Epoch;
use awaken_session_execution_contract::ThreadLiveObservation;
use awaken_sync::LeafLock;
use awaken_time::mono::MonoDeadline;
use futures_lite::StreamExt as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

const BACKLOG: usize = 128;
const MAX_IDENTITIES: usize = 4096;
const MAX_BYTES: usize = 96 * 1024;
const MAX_AGE: std::time::Duration = std::time::Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

struct Outgoing {
    packet: Packet,
    deadline: MonoDeadline,
}

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
    outgoing: mpsc::Sender<Outgoing>,
    sequences: LeafLock<HashMap<Response, (Uuid, u64)>>,
    task: JoinHandle<()>,
    #[cfg(test)]
    ready: Arc<tokio::sync::Notify>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
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
        let _ = self.outgoing.try_send(Outgoing {
            packet: packet.clone(),
            deadline: MonoDeadline::after(MAX_AGE),
        });
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
    /// NATS subject. Start the relay independently of local Hub readiness.
    pub fn attach_nats_preview(self: &Arc<Self>, url: &str, subject: &str) -> Result<(), String> {
        if subject.is_empty()
            || subject.split('.').any(|part| part.is_empty())
            || subject
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'*' | b'>'))
        {
            return Err("preview relay requires a literal NATS subject".into());
        }
        let address = url
            .parse::<async_nats::ServerAddr>()
            .map_err(|_| "invalid preview relay NATS address")?;
        let source = Uuid::new_v4();
        let (outgoing, mut pending) = mpsc::channel::<Outgoing>(BACKLOG);
        let subject = subject.to_owned();
        let hub = Arc::downgrade(self);
        #[cfg(test)]
        let ready = Arc::new(tokio::sync::Notify::new());
        #[cfg(test)]
        let connected = ready.clone();
        let task = tokio::spawn(async move {
            loop {
                let connection = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    connect(address.clone(), &subject, source),
                )
                .await;
                match connection {
                    Ok(Ok((client, messages))) => {
                        tracing::debug!("cross-Coordinator previews available");
                        #[cfg(test)]
                        connected.notify_one();
                        forward(&hub, &subject, source, client, messages, &mut pending).await;
                    }
                    Ok(Err(error)) => {
                        tracing::debug!(error, "cross-Coordinator previews unavailable")
                    }
                    Err(_) => tracing::debug!("cross-Coordinator preview connection timed out"),
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        });
        self.relay
            .set(Relay {
                source,
                outgoing,
                sequences: Default::default(),
                task,
                #[cfg(test)]
                ready,
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
            && channel.observe_preview_epoch(&response.run, epoch)
            && channel.previews.lock().accept(response, &packet)
        {
            let _ = channel
                .sender
                .send(ThreadEvent::ClaimedLive(ThreadLiveObservation {
                    observation: packet.observation,
                    epoch: Some(epoch),
                }));
        }
    }
}

async fn connect(
    address: async_nats::ServerAddr,
    subject: &str,
    source: Uuid,
) -> Result<(async_nats::Client, async_nats::Subscriber), &'static str> {
    let client = async_nats::ConnectOptions::new()
        .subscription_capacity(BACKLOG)
        .client_capacity(BACKLOG)
        .connect(address)
        .await
        .map_err(|_| "connect")?;
    let mut messages = client
        .subscribe(subject.to_owned())
        .await
        .map_err(|_| "subscribe")?;
    // Confirm that the broker processed our subscription and permits relay I/O.
    let probe = format!("preview-probe:{source}");
    client
        .publish(subject.to_owned(), probe.clone().into())
        .await
        .map_err(|_| "publish probe")?;
    while let Some(message) = messages.next().await {
        if message.payload.as_ref() == probe.as_bytes() {
            return Ok((client, messages));
        }
    }
    Err("subscription closed before probe")
}

async fn forward(
    hub: &Weak<ThreadEventHub>,
    subject: &str,
    source: Uuid,
    client: async_nats::Client,
    mut messages: async_nats::Subscriber,
    pending: &mut mpsc::Receiver<Outgoing>,
) {
    let receive = async {
        while let Some(message) = messages.next().await {
            let Some(hub) = hub.upgrade() else { break };
            if message.payload.len() <= MAX_BYTES
                && let Ok(packet) = serde_json::from_slice::<Packet>(&message.payload)
                && packet.source != source
            {
                hub.receive_preview(packet);
            }
        }
    };
    let send = async {
        while let Some(Outgoing { packet, deadline }) = pending.recv().await {
            if !deadline.has_passed()
                && let Ok(body) = serde_json::to_vec(&packet)
                && body.len() <= MAX_BYTES
                && client.connection_state() == async_nats::connection::State::Connected
            {
                let _ = tokio::time::timeout_at(
                    deadline.instant(),
                    client.publish(subject.to_owned(), body.into()),
                )
                .await;
            }
        }
    };
    tokio::select! {
        () = receive => {},
        () = send => {},
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
        hub.receive_preview(packet(5, 1, "replacement prefix lost"));
        hub.receive_preview(packet(4, 1, "old trailing frame"));

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
    async fn a_broker_without_a_handshake_does_not_block_local_previews() {
        let broker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("nats://{}", broker.local_addr().unwrap());
        let hub = Arc::new(ThreadEventHub::new());
        hub.attach_nats_preview(&address, "awaken.preview.test")
            .unwrap();
        let mut subscription = hub.live_subscription("thread").unwrap();
        let claim = awaken_worker_contract::RunClaim {
            run_id: RunId("preview-run".into()),
            owner: "worker".into(),
            epoch: 7,
        };
        awaken_dispatch_contract::with_preview_claim_epoch(&claim, async {
            hub.publish("thread", ThreadEvent::Live(text("thread", "local")));
        })
        .await;
        let live = tokio::time::timeout(Duration::from_secs(2), subscription.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(live.observation, text("thread", "local"));
    }

    #[tokio::test]
    #[ignore = "requires an isolated Core NATS broker and AWAKEN_TEST_PREVIEW_NATS_URL"]
    async fn core_nats_relay_carries_previews_between_independent_hubs() {
        let broker = std::env::var("AWAKEN_TEST_PREVIEW_NATS_URL").expect("real Core NATS fixture");
        let subject = format!("awaken.preview.tests.{}", Uuid::new_v4());
        let a = Arc::new(ThreadEventHub::new());
        let b = Arc::new(ThreadEventHub::new());
        let c = Arc::new(ThreadEventHub::new());
        a.attach_nats_preview(&broker, &subject).unwrap();
        b.attach_nats_preview(&broker, &subject).unwrap();
        c.attach_nats_preview(&broker, &format!("{subject}.isolated"))
            .unwrap();
        for hub in [&a, &b, &c] {
            tokio::time::timeout(CONNECT_TIMEOUT, hub.relay.get().unwrap().ready.notified())
                .await
                .expect("the isolated broker processed the subscription probe");
        }
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

    #[tokio::test]
    #[ignore = "requires AWAKEN_TEST_PREVIEW_NATS_SERVER pointing to a local nats-server binary"]
    async fn core_nats_can_start_late_and_restart_without_recreating_the_hubs() {
        struct Broker(std::process::Child);
        impl Drop for Broker {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let binary = std::env::var_os("AWAKEN_TEST_PREVIEW_NATS_SERVER").unwrap();
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        let start = || {
            Broker(
                std::process::Command::new(&binary)
                    .args(["-a", "127.0.0.1", "-p", &address.port().to_string()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        };
        let url = format!("nats://{address}");
        let a = Arc::new(ThreadEventHub::new());
        let b = Arc::new(ThreadEventHub::new());
        a.attach_nats_preview(&url, "awaken.preview.restart")
            .unwrap();
        b.attach_nats_preview(&url, "awaken.preview.restart")
            .unwrap();
        let mut local = a.live_subscription("thread").unwrap();
        let mut remote = b.live_subscription("thread").unwrap();
        let claim = awaken_worker_contract::RunClaim {
            run_id: RunId("preview-run".into()),
            owner: "worker".into(),
            epoch: 7,
        };
        let publish = async |response, label: &'static str| {
            let mut preview = text("thread", label);
            preview.assistant_response.as_mut().unwrap().response = response;
            awaken_dispatch_contract::with_preview_claim_epoch(&claim, async {
                a.publish("thread", ThreadEvent::Live(preview));
            })
            .await;
        };
        let wait = async |mut response: usize, remote: &mut dyn awaken_session_execution_contract::SessionThreadLiveSubscription| {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    publish(response, "online").await;
                    response += 1;
                    if let Ok(frame) = tokio::time::timeout(Duration::from_millis(100), remote.recv()).await {
                        let frame = frame.unwrap().unwrap();
                        assert_eq!(frame.observation.event.kind, text("thread", "online").event.kind);
                        break;
                    }
                }
            }).await.expect("existing relay reconnects");
        };
        publish(0, "before broker startup").await;
        assert!(local.recv().await.unwrap().is_some());
        assert!(tokio::time::timeout(MAX_AGE, remote.recv()).await.is_err());
        drop(reservation);
        let broker = start();
        wait(1, remote.as_mut()).await;
        drop(local);
        drop(broker);
        let mut local = a.live_subscription("thread").unwrap();
        while let Ok(Ok(Some(_))) = tokio::time::timeout(MAX_AGE, remote.recv()).await {}
        publish(1000, "while broker is down").await;
        assert!(local.recv().await.unwrap().is_some());
        assert!(tokio::time::timeout(MAX_AGE, remote.recv()).await.is_err());
        let _broker = start();
        wait(1001, remote.as_mut()).await;
    }
}
