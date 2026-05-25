//! Optional ProtocolReplayLog and projector relay attachments.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use awaken_contract::contract::event_store::EventLookup;
use awaken_contract::contract::outbox::{
    OUTBOX_LANE_CANONICAL, OUTBOX_LANE_PROTOCOL_REPLAY, OUTBOX_TARGET_PROTOCOL_FANOUT,
    OUTBOX_TARGET_PROTOCOL_PROJECTOR, OutboxStore,
};
use awaken_contract::contract::protocol_replay_log::{ProtocolReplayLog, ProtocolReplayLookup};
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::app::{ReplayBufferEntry, ReplayBufferMap, ServerState};
use crate::outbox_relay::{OutboxRelay, OutboxRelayConfig, OutboxRelayError};
use crate::protocol_fanout::{ProtocolReplayFanoutPublisher, ProtocolReplayFanoutRelayHandler};
use crate::protocol_projector::CanonicalOutboxProtocolProjector;

type ProtocolReplayRegistry = HashMap<usize, ProtocolReplayAttachment>;

#[derive(Clone)]
struct ProtocolReplayAttachment {
    replay_buffers: Weak<Mutex<HashMap<String, ReplayBufferEntry>>>,
    log: Option<Arc<dyn ProtocolReplayLog>>,
    projector_relay: Option<ProtocolProjectorRelayAttachment>,
    fanout_relay: Option<ProtocolFanoutRelayAttachment>,
}

#[derive(Clone)]
struct ProtocolProjectorRelayAttachment {
    outbox: Arc<dyn OutboxStore>,
    event_lookup: Arc<dyn EventLookup>,
    replay_writer: Arc<dyn awaken_contract::contract::protocol_replay_log::ProtocolReplayWriter>,
    config: ProtocolProjectorRelayConfig,
}

#[derive(Clone)]
struct ProtocolFanoutRelayAttachment {
    outbox: Arc<dyn OutboxStore>,
    replay_lookup: Arc<dyn ProtocolReplayLookup>,
    publisher: Arc<dyn ProtocolReplayFanoutPublisher>,
    config: ProtocolFanoutRelayConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolProjectorRelayConfig {
    pub relay: OutboxRelayConfig,
    pub idle_sleep: Duration,
    pub error_sleep: Duration,
}

impl Default for ProtocolProjectorRelayConfig {
    fn default() -> Self {
        Self {
            relay: OutboxRelayConfig {
                lane: OUTBOX_LANE_CANONICAL.to_string(),
                target: OUTBOX_TARGET_PROTOCOL_PROJECTOR.to_string(),
                consumer_id: "protocol-projector".to_string(),
                batch_limit: 100,
                lease_ms: 30_000,
                retry_delay_ms: 1_000,
                max_retry_delay_ms: 30_000,
            },
            idle_sleep: Duration::from_millis(250),
            error_sleep: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolFanoutRelayConfig {
    pub relay: OutboxRelayConfig,
    pub idle_sleep: Duration,
    pub error_sleep: Duration,
}

impl Default for ProtocolFanoutRelayConfig {
    fn default() -> Self {
        Self {
            relay: OutboxRelayConfig {
                lane: OUTBOX_LANE_PROTOCOL_REPLAY.to_string(),
                target: OUTBOX_TARGET_PROTOCOL_FANOUT.to_string(),
                consumer_id: "protocol-fanout".to_string(),
                batch_limit: 100,
                lease_ms: 30_000,
                retry_delay_ms: 1_000,
                max_retry_delay_ms: 30_000,
            },
            idle_sleep: Duration::from_millis(250),
            error_sleep: Duration::from_secs(1),
        }
    }
}

pub struct ProtocolRelayHandle {
    task: JoinHandle<()>,
    cancel: CancellationToken,
    name: &'static str,
}

pub type ProtocolProjectorRelayHandle = ProtocolRelayHandle;
pub type ProtocolFanoutRelayHandle = ProtocolRelayHandle;

impl ProtocolRelayHandle {
    pub async fn shutdown(self) {
        self.shutdown_with_timeout(Duration::from_secs(30)).await;
    }

    pub async fn shutdown_with_timeout(self, timeout: Duration) {
        self.cancel.cancel();
        let mut task = self.task;
        match tokio::time::timeout(timeout, &mut task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) if error.is_cancelled() => {}
            Ok(Err(error)) => {
                tracing::warn!(error = %error, relay = self.name, "outbox relay failed during shutdown");
            }
            Err(_) => {
                tracing::warn!(
                    relay = self.name,
                    timeout_ms = timeout.as_millis(),
                    "outbox relay shutdown timed out; aborting task and relying on lease retry"
                );
                task.abort();
                if let Err(error) = task.await
                    && !error.is_cancelled()
                {
                    tracing::warn!(error = %error, relay = self.name, "outbox relay failed after abort");
                }
            }
        }
    }
}

pub(crate) struct ProtocolRelayHandles {
    handles: Vec<ProtocolRelayHandle>,
}

impl ProtocolRelayHandles {
    pub async fn shutdown(self) {
        for handle in self.handles {
            handle.shutdown().await;
        }
    }
}

static PROTOCOL_REPLAY_REGISTRY: OnceLock<Mutex<ProtocolReplayRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<ProtocolReplayRegistry> {
    PROTOCOL_REPLAY_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(replay_buffers: &ReplayBufferMap) -> usize {
    Arc::as_ptr(replay_buffers) as usize
}

fn prune(registry: &mut ProtocolReplayRegistry) {
    registry.retain(|_, attachment| attachment.replay_buffers.upgrade().is_some());
}

#[must_use]
pub fn with_protocol_replay_log(
    state: ServerState,
    log: Arc<dyn ProtocolReplayLog>,
) -> ServerState {
    let mut registry = registry().lock();
    prune(&mut registry);
    let weak = Arc::downgrade(&state.protocol.replay_buffers);
    registry
        .entry(key(&state.protocol.replay_buffers))
        .and_modify(|attachment| attachment.log = Some(log.clone()))
        .or_insert_with(|| ProtocolReplayAttachment {
            replay_buffers: weak,
            log: Some(log),
            projector_relay: None,
            fanout_relay: None,
        });
    state
}

pub fn protocol_replay_log(state: &ServerState) -> Option<Arc<dyn ProtocolReplayLog>> {
    protocol_replay_log_for_buffers(&state.protocol.replay_buffers)
}

pub fn protocol_replay_log_for_buffers(
    replay_buffers: &ReplayBufferMap,
) -> Option<Arc<dyn ProtocolReplayLog>> {
    let mut registry = registry().lock();
    prune(&mut registry);
    registry
        .get(&key(replay_buffers))
        .and_then(|attachment| attachment.log.as_ref().map(Arc::clone))
}

pub fn with_protocol_projector_relay(
    state: ServerState,
    outbox: Arc<dyn OutboxStore>,
    event_lookup: Arc<dyn EventLookup>,
    replay_writer: Arc<dyn awaken_contract::contract::protocol_replay_log::ProtocolReplayWriter>,
    config: ProtocolProjectorRelayConfig,
) -> Result<ServerState, OutboxRelayError> {
    config.relay.validate()?;
    let mut registry = registry().lock();
    prune(&mut registry);
    let weak = Arc::downgrade(&state.protocol.replay_buffers);
    registry
        .entry(key(&state.protocol.replay_buffers))
        .and_modify(|attachment| {
            attachment.projector_relay = Some(ProtocolProjectorRelayAttachment {
                outbox: outbox.clone(),
                event_lookup: event_lookup.clone(),
                replay_writer: replay_writer.clone(),
                config: config.clone(),
            });
        })
        .or_insert_with(|| ProtocolReplayAttachment {
            replay_buffers: weak,
            log: None,
            projector_relay: Some(ProtocolProjectorRelayAttachment {
                outbox,
                event_lookup,
                replay_writer,
                config,
            }),
            fanout_relay: None,
        });
    Ok(state)
}

pub fn with_protocol_fanout_relay(
    state: ServerState,
    outbox: Arc<dyn OutboxStore>,
    replay_lookup: Arc<dyn ProtocolReplayLookup>,
    publisher: Arc<dyn ProtocolReplayFanoutPublisher>,
    config: ProtocolFanoutRelayConfig,
) -> Result<ServerState, OutboxRelayError> {
    config.relay.validate()?;
    let mut registry = registry().lock();
    prune(&mut registry);
    let weak = Arc::downgrade(&state.protocol.replay_buffers);
    registry
        .entry(key(&state.protocol.replay_buffers))
        .and_modify(|attachment| {
            attachment.fanout_relay = Some(ProtocolFanoutRelayAttachment {
                outbox: outbox.clone(),
                replay_lookup: replay_lookup.clone(),
                publisher: publisher.clone(),
                config: config.clone(),
            });
        })
        .or_insert_with(|| ProtocolReplayAttachment {
            replay_buffers: weak,
            log: None,
            projector_relay: None,
            fanout_relay: Some(ProtocolFanoutRelayAttachment {
                outbox,
                replay_lookup,
                publisher,
                config,
            }),
        });
    Ok(state)
}

pub(crate) async fn start_protocol_relays(
    state: &ServerState,
) -> Result<ProtocolRelayHandles, OutboxRelayError> {
    let mut handles = Vec::new();
    if let Some(handle) = start_protocol_projector_relay(state)? {
        handles.push(handle);
    }
    match start_protocol_fanout_relay(state) {
        Ok(Some(handle)) => handles.push(handle),
        Ok(None) => {}
        Err(error) => {
            ProtocolRelayHandles { handles }.shutdown().await;
            return Err(error);
        }
    }
    Ok(ProtocolRelayHandles { handles })
}

pub(crate) fn start_protocol_projector_relay(
    state: &ServerState,
) -> Result<Option<ProtocolProjectorRelayHandle>, OutboxRelayError> {
    let attachment = {
        let mut registry = registry().lock();
        prune(&mut registry);
        registry
            .get(&key(&state.protocol.replay_buffers))
            .and_then(|attachment| attachment.projector_relay.clone())
    };
    let Some(attachment) = attachment else {
        return Ok(None);
    };
    let handler = Arc::new(CanonicalOutboxProtocolProjector::new_all_protocols(
        attachment.event_lookup,
        attachment.replay_writer,
    ));
    let relay = OutboxRelay::new(attachment.outbox, handler, attachment.config.relay.clone())?;
    let config = attachment.config;
    let cancel = CancellationToken::new();
    Ok(Some(ProtocolRelayHandle {
        task: tokio::spawn(run_outbox_relay(
            relay,
            config.idle_sleep,
            config.error_sleep,
            "protocol projector relay",
            cancel.clone(),
        )),
        cancel,
        name: "protocol projector relay",
    }))
}

pub(crate) fn start_protocol_fanout_relay(
    state: &ServerState,
) -> Result<Option<ProtocolFanoutRelayHandle>, OutboxRelayError> {
    let attachment = {
        let mut registry = registry().lock();
        prune(&mut registry);
        registry
            .get(&key(&state.protocol.replay_buffers))
            .and_then(|attachment| attachment.fanout_relay.clone())
    };
    let Some(attachment) = attachment else {
        return Ok(None);
    };
    let handler = Arc::new(ProtocolReplayFanoutRelayHandler::new(
        attachment.replay_lookup,
        attachment.publisher,
    ));
    let relay = OutboxRelay::new(attachment.outbox, handler, attachment.config.relay.clone())?;
    let config = attachment.config;
    let cancel = CancellationToken::new();
    Ok(Some(ProtocolRelayHandle {
        task: tokio::spawn(run_outbox_relay(
            relay,
            config.idle_sleep,
            config.error_sleep,
            "protocol fanout relay",
            cancel.clone(),
        )),
        cancel,
        name: "protocol fanout relay",
    }))
}

async fn run_outbox_relay(
    relay: OutboxRelay,
    idle_sleep: Duration,
    error_sleep: Duration,
    name: &'static str,
    cancel: CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            break;
        }
        match relay.tick().await {
            Ok(stats) if stats.claimed == 0 => wait(idle_sleep).await,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(error = %error, relay = name, "outbox relay tick failed");
                wait(error_sleep).await;
            }
        }
        if cancel.is_cancelled() {
            break;
        }
    }
}

async fn wait(duration: Duration) {
    if duration.is_zero() {
        tokio::task::yield_now().await;
    } else {
        tokio::time::sleep(duration).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_contract::contract::durable_event_sink::{
        AgentEventNormalizationContext, AgentEventNormalizer, ScopedAgentEventNormalizer,
    };
    use awaken_contract::contract::event::AgentEvent;
    use awaken_contract::contract::event_store::{AppendOptions, EventWriter};
    use awaken_contract::contract::outbox::{OutboxMessageDraft, OutboxStatus};
    use awaken_contract::contract::protocol_replay_log::{
        ProtocolReplayDraft, ProtocolReplayReader, ProtocolReplayWriter, ProtocolStreamKey,
    };
    use awaken_contract::contract::storage::ThreadRunStore;
    use awaken_runtime::{AgentRuntime, RuntimeError};
    use awaken_stores::{
        InMemoryEventStore, InMemoryMailboxStore, InMemoryOutboxStore, InMemoryProtocolReplayLog,
        InMemoryStore,
    };

    use crate::app::{ServerConfig, ServerState};
    use crate::mailbox::{Mailbox, MailboxConfig};
    use crate::protocol_fanout::{
        ProtocolReplayFanoutError, ProtocolReplayFanoutMessage, ProtocolReplayFanoutPublisher,
    };
    use crate::protocol_projector::{AI_SDK_PROTOCOL, AI_SDK_PROTOCOL_VERSION};

    struct StubResolver;

    impl awaken_runtime::AgentResolver for StubResolver {
        fn resolve(&self, agent_id: &str) -> Result<awaken_runtime::ResolvedAgent, RuntimeError> {
            Err(RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
    }

    fn make_state() -> ServerState {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
        let store = Arc::new(InMemoryStore::new());
        let mailbox_store = Arc::new(InMemoryMailboxStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            mailbox_store,
            store.clone(),
            "test".to_string(),
            MailboxConfig::default(),
        ));
        ServerState::new(
            runtime,
            mailbox,
            store as Arc<dyn ThreadRunStore>,
            Arc::new(StubResolver),
            ServerConfig::default(),
        )
    }

    async fn append_run_start(event_store: &InMemoryEventStore) -> String {
        let normalizer = ScopedAgentEventNormalizer::new(
            AgentEventNormalizationContext::new("thread-relay", "run-relay", "test").unwrap(),
        );
        let normalized = normalizer
            .normalize(&AgentEvent::RunStart {
                thread_id: "thread-relay".into(),
                run_id: "run-relay".into(),
                parent_run_id: None,
                identity: None,
            })
            .unwrap()
            .unwrap();
        event_store
            .append(normalized.draft, AppendOptions::default())
            .await
            .unwrap()
            .event
            .event_id
            .as_str()
            .to_string()
    }

    fn fast_config() -> ProtocolProjectorRelayConfig {
        ProtocolProjectorRelayConfig {
            idle_sleep: Duration::from_millis(1),
            error_sleep: Duration::from_millis(1),
            ..ProtocolProjectorRelayConfig::default()
        }
    }

    fn fast_fanout_config() -> ProtocolFanoutRelayConfig {
        ProtocolFanoutRelayConfig {
            idle_sleep: Duration::from_millis(1),
            error_sleep: Duration::from_millis(1),
            ..ProtocolFanoutRelayConfig::default()
        }
    }

    async fn replay_count(log: &InMemoryProtocolReplayLog) -> usize {
        log.list_replay(
            ProtocolStreamKey::new(
                "thread:thread-relay",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap()
        .records
        .len()
    }

    #[derive(Default)]
    struct RecordingFanoutPublisher {
        replay_ids: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ProtocolReplayFanoutPublisher for RecordingFanoutPublisher {
        async fn publish(
            &self,
            message: ProtocolReplayFanoutMessage,
        ) -> Result<(), ProtocolReplayFanoutError> {
            self.replay_ids
                .lock()
                .push(message.record.protocol_replay_id.as_str().to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn protocol_projector_relay_projects_attached_outbox_in_background() {
        let event_store = Arc::new(InMemoryEventStore::new());
        let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
        let outbox = Arc::new(InMemoryOutboxStore::new());
        let event_id = append_run_start(&event_store).await;
        let mut draft = OutboxMessageDraft::new(
            OUTBOX_LANE_CANONICAL,
            OUTBOX_TARGET_PROTOCOL_PROJECTOR,
            serde_json::json!({ "event_id": event_id }),
        )
        .unwrap();
        draft.dedupe_key = Some(format!("canonical/{event_id}"));
        outbox.enqueue_outbox(draft).await.unwrap();
        let state = with_protocol_replay_log(make_state(), replay_log.clone());
        let state = with_protocol_projector_relay(
            state,
            outbox.clone(),
            event_store as Arc<dyn EventLookup>,
            replay_log.clone() as Arc<dyn ProtocolReplayWriter>,
            fast_config(),
        )
        .unwrap();
        assert!(protocol_replay_log(&state).is_some());

        let handle = start_protocol_projector_relay(&state).unwrap().unwrap();
        for _ in 0..50 {
            if replay_count(&replay_log).await == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.shutdown().await;

        assert_eq!(replay_count(&replay_log).await, 2);
        let delivered = outbox
            .list_outbox(Some(OutboxStatus::Delivered), 10)
            .await
            .unwrap();
        assert_eq!(delivered.len(), 1);
    }

    #[tokio::test]
    async fn protocol_fanout_relay_publishes_attached_outbox_in_background() {
        let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
        let outbox = Arc::new(InMemoryOutboxStore::new());
        let publisher = Arc::new(RecordingFanoutPublisher::default());
        let record = replay_log
            .append_replay(
                ProtocolReplayDraft::new(
                    "thread:thread-fanout-state",
                    AI_SDK_PROTOCOL,
                    AI_SDK_PROTOCOL_VERSION,
                    "ai-sdk-projector-v1",
                    "wire-fanout-state",
                    "start",
                    b"data: start\n\n".to_vec(),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .record;
        outbox
            .enqueue_outbox(
                OutboxMessageDraft::new(
                    OUTBOX_LANE_PROTOCOL_REPLAY,
                    OUTBOX_TARGET_PROTOCOL_FANOUT,
                    serde_json::json!({
                        "protocol_replay_id": record.protocol_replay_id.as_str(),
                        "protocol": record.protocol.as_str(),
                        "protocol_version": record.protocol_version.as_str(),
                        "wire_event_id": record.wire_event_id.as_str(),
                    }),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let state = with_protocol_fanout_relay(
            make_state(),
            outbox.clone(),
            replay_log,
            publisher.clone(),
            fast_fanout_config(),
        )
        .unwrap();

        let handles = start_protocol_relays(&state).await.unwrap();
        for _ in 0..50 {
            if publisher.replay_ids.lock().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handles.shutdown().await;

        assert_eq!(publisher.replay_ids.lock().len(), 1);
        let delivered = outbox
            .list_outbox(Some(OutboxStatus::Delivered), 10)
            .await
            .unwrap();
        assert_eq!(delivered.len(), 1);
    }

    // Regression: previously `run_outbox_relay` raced `relay.tick()` against
    // `cancel.cancelled()` in the same `select!`, so a shutdown that fired
    // after `claim_outbox` but before `ack`/`nack` would drop the tick
    // future and leave the row claimed until lease expiry. The relay now
    // only observes cancellation between ticks; verify a slow handler
    // completes its delivery before shutdown returns.
    #[tokio::test]
    async fn shutdown_does_not_drop_in_flight_tick() {
        use awaken_contract::contract::outbox::OutboxStore;
        use tokio::sync::Notify;

        struct GatedHandler {
            entered: Arc<Notify>,
            release: Arc<Notify>,
        }

        #[async_trait]
        impl crate::outbox_relay::OutboxRelayHandler for GatedHandler {
            async fn deliver(
                &self,
                _message: &awaken_contract::contract::outbox::OutboxMessage,
            ) -> Result<(), crate::outbox_relay::OutboxRelayError> {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(())
            }
        }

        let outbox = Arc::new(InMemoryOutboxStore::new());
        let mut draft = OutboxMessageDraft::new(
            OUTBOX_LANE_CANONICAL,
            OUTBOX_TARGET_PROTOCOL_PROJECTOR,
            serde_json::json!({"event_id": "evt"}),
        )
        .unwrap();
        draft.dedupe_key = Some("dedupe".into());
        outbox.enqueue_outbox(draft).await.unwrap();

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let handler = Arc::new(GatedHandler {
            entered: entered.clone(),
            release: release.clone(),
        });
        let relay = OutboxRelay::new(
            outbox.clone(),
            handler,
            OutboxRelayConfig {
                lane: OUTBOX_LANE_CANONICAL.to_string(),
                target: OUTBOX_TARGET_PROTOCOL_PROJECTOR.to_string(),
                consumer_id: "shutdown-test".into(),
                batch_limit: 10,
                lease_ms: 60_000,
                retry_delay_ms: 0,
                max_retry_delay_ms: 0,
            },
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let mut task = tokio::spawn(run_outbox_relay(
            relay,
            Duration::from_millis(1),
            Duration::from_millis(1),
            "shutdown-test",
            cancel.clone(),
        ));

        // Wait until the handler is mid-deliver, then request shutdown.
        entered.notified().await;
        cancel.cancel();

        // While the handler is blocked, the relay task must still be alive:
        // cancel-safe shutdown means the tick future is not dropped, so the
        // task cannot exit until the handler returns.
        let early = tokio::time::timeout(Duration::from_millis(25), &mut task).await;
        assert!(
            early.is_err(),
            "relay task exited mid-deliver, lost cancel-safety"
        );

        // Let the handler complete; the relay should ack then observe the
        // cancellation between ticks and shut down cleanly.
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("relay task did not shut down after handler released")
            .expect("relay task panicked");

        let delivered = outbox
            .list_outbox(Some(OutboxStatus::Delivered), 10)
            .await
            .unwrap();
        assert_eq!(delivered.len(), 1, "row must be acked, not stuck claimed");
    }

    #[tokio::test]
    async fn shutdown_timeout_bounds_stuck_in_flight_tick() {
        use awaken_contract::contract::outbox::OutboxStore;
        use tokio::sync::Notify;

        struct StuckHandler {
            entered: Arc<Notify>,
        }

        #[async_trait]
        impl crate::outbox_relay::OutboxRelayHandler for StuckHandler {
            async fn deliver(
                &self,
                _message: &awaken_contract::contract::outbox::OutboxMessage,
            ) -> Result<(), crate::outbox_relay::OutboxRelayError> {
                self.entered.notify_one();
                std::future::pending::<()>().await;
                Ok(())
            }
        }

        let outbox = Arc::new(InMemoryOutboxStore::new());
        let mut draft = OutboxMessageDraft::new(
            OUTBOX_LANE_CANONICAL,
            OUTBOX_TARGET_PROTOCOL_PROJECTOR,
            serde_json::json!({"event_id": "evt-timeout"}),
        )
        .unwrap();
        draft.dedupe_key = Some("dedupe-timeout".into());
        outbox.enqueue_outbox(draft).await.unwrap();

        let entered = Arc::new(Notify::new());
        let relay = OutboxRelay::new(
            outbox.clone(),
            Arc::new(StuckHandler {
                entered: entered.clone(),
            }),
            OutboxRelayConfig {
                lane: OUTBOX_LANE_CANONICAL.to_string(),
                target: OUTBOX_TARGET_PROTOCOL_PROJECTOR.to_string(),
                consumer_id: "shutdown-timeout-test".into(),
                batch_limit: 10,
                lease_ms: 60_000,
                retry_delay_ms: 0,
                max_retry_delay_ms: 0,
            },
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let handle = ProtocolRelayHandle {
            task: tokio::spawn(run_outbox_relay(
                relay,
                Duration::from_millis(1),
                Duration::from_millis(1),
                "shutdown-timeout-test",
                cancel.clone(),
            )),
            cancel,
            name: "shutdown-timeout-test",
        };

        entered.notified().await;
        tokio::time::timeout(
            Duration::from_secs(1),
            handle.shutdown_with_timeout(Duration::from_millis(25)),
        )
        .await
        .expect("shutdown timeout must bound a stuck handler");

        let claimed = outbox
            .list_outbox(Some(OutboxStatus::Claimed), 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1, "lease retry owns recovery after abort");
    }
}
