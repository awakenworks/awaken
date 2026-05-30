//! Optional ProtocolReplayLog and projector relay attachments.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use awaken_contract::contract::event_store::EventLookup;
use awaken_contract::contract::outbox::{
    OUTBOX_LANE_CANONICAL, OUTBOX_LANE_PROTOCOL_REPLAY, OUTBOX_TARGET_A2A_WEBHOOK,
    OUTBOX_TARGET_PROTOCOL_FANOUT, OUTBOX_TARGET_PROTOCOL_PROJECTOR, OutboxStore,
};
use awaken_contract::contract::protocol_replay_log::{ProtocolReplayLog, ProtocolReplayLookup};
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::app::{ReplayBufferEntry, ReplayBufferMap, ServerState};
use crate::outbox_relay::{OutboxRelay, OutboxRelayConfig, OutboxRelayError};
use crate::protocol_fanout::{ProtocolReplayFanoutPublisher, ProtocolReplayFanoutRelayHandler};
use crate::protocol_projector::CanonicalOutboxProtocolProjector;
use crate::protocols::a2a::push_outbox::A2aPushWebhookRelayHandler;

type ProtocolReplayRegistry = HashMap<usize, ProtocolReplayAttachment>;

#[derive(Clone)]
struct ProtocolReplayAttachment {
    replay_buffers: Weak<Mutex<HashMap<String, ReplayBufferEntry>>>,
    log: Option<Arc<dyn ProtocolReplayLog>>,
    projector_relay: Option<ProtocolProjectorRelayAttachment>,
    fanout_relay: Option<ProtocolFanoutRelayAttachment>,
    a2a_push_relay: Option<A2aPushWebhookRelayAttachment>,
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

#[derive(Clone)]
struct A2aPushWebhookRelayAttachment {
    outbox: Arc<dyn OutboxStore>,
    config: A2aPushWebhookRelayConfig,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aPushWebhookRelayConfig {
    pub relay: OutboxRelayConfig,
    pub idle_sleep: Duration,
    pub error_sleep: Duration,
}

impl Default for A2aPushWebhookRelayConfig {
    fn default() -> Self {
        Self {
            relay: OutboxRelayConfig {
                lane: OUTBOX_LANE_PROTOCOL_REPLAY.to_string(),
                target: OUTBOX_TARGET_A2A_WEBHOOK.to_string(),
                consumer_id: "a2a-push-webhook".to_string(),
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
pub type A2aPushWebhookRelayHandle = ProtocolRelayHandle;

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
            a2a_push_relay: None,
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
            a2a_push_relay: None,
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
            a2a_push_relay: None,
        });
    Ok(state)
}

pub fn with_a2a_push_webhook_relay(
    mut state: ServerState,
    outbox: Arc<dyn OutboxStore>,
    config: A2aPushWebhookRelayConfig,
) -> Result<ServerState, OutboxRelayError> {
    state.protocol.a2a_push_outbox = outbox.clone();
    state.protocol.a2a_push_relay_config = config.clone();
    register_a2a_push_webhook_relay_for_buffers(&state.protocol.replay_buffers, outbox, config)?;
    Ok(state)
}

pub(crate) fn register_a2a_push_webhook_relay_for_buffers(
    replay_buffers: &ReplayBufferMap,
    outbox: Arc<dyn OutboxStore>,
    config: A2aPushWebhookRelayConfig,
) -> Result<(), OutboxRelayError> {
    config.relay.validate()?;
    let mut registry = registry().lock();
    prune(&mut registry);
    let weak = Arc::downgrade(replay_buffers);
    registry
        .entry(key(replay_buffers))
        .and_modify(|attachment| {
            attachment.a2a_push_relay = Some(A2aPushWebhookRelayAttachment {
                outbox: outbox.clone(),
                config: config.clone(),
            });
        })
        .or_insert_with(|| ProtocolReplayAttachment {
            replay_buffers: weak,
            log: None,
            projector_relay: None,
            fanout_relay: None,
            a2a_push_relay: Some(A2aPushWebhookRelayAttachment { outbox, config }),
        });
    Ok(())
}

/// Move every relay attachment (log, projector, fanout, A2A push) from `old`
/// replay buffers to `new`.
///
/// The registry is keyed by replay-buffer identity, so replacing a state's
/// protocol module — which swaps in fresh `replay_buffers` — would otherwise
/// orphan any attachment registered before the swap. `start_protocol_relays`
/// looks attachments up by the current buffers, so an orphaned projector/fanout/
/// log relay would silently never start.
pub(crate) fn migrate_protocol_attachments(old: &ReplayBufferMap, new: &ReplayBufferMap) {
    if Arc::ptr_eq(old, new) {
        return;
    }
    let mut registry = registry().lock();
    prune(&mut registry);
    let Some(mut attachment) = registry.remove(&key(old)) else {
        return;
    };
    attachment.replay_buffers = Arc::downgrade(new);
    registry.insert(key(new), attachment);
}

pub fn a2a_push_webhook_outbox_for_buffers(
    replay_buffers: &ReplayBufferMap,
) -> Option<Arc<dyn OutboxStore>> {
    let mut registry = registry().lock();
    prune(&mut registry);
    registry
        .get(&key(replay_buffers))
        .and_then(|attachment| attachment.a2a_push_relay.as_ref())
        .map(|attachment| Arc::clone(&attachment.outbox))
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
    match start_a2a_push_webhook_relay(state) {
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

pub(crate) fn start_a2a_push_webhook_relay(
    state: &ServerState,
) -> Result<Option<A2aPushWebhookRelayHandle>, OutboxRelayError> {
    let attachment = {
        let mut registry = registry().lock();
        prune(&mut registry);
        registry
            .get(&key(&state.protocol.replay_buffers))
            .and_then(|attachment| attachment.a2a_push_relay.clone())
    };
    let Some(attachment) = attachment else {
        return Ok(None);
    };
    let handler = Arc::new(A2aPushWebhookRelayHandler::new(reqwest::Client::new()));
    let relay = OutboxRelay::new(attachment.outbox, handler, attachment.config.relay.clone())?;
    let config = attachment.config;
    let cancel = CancellationToken::new();
    Ok(Some(ProtocolRelayHandle {
        task: tokio::spawn(run_outbox_relay(
            relay,
            config.idle_sleep,
            config.error_sleep,
            "a2a push webhook relay",
            cancel.clone(),
        )),
        cancel,
        name: "a2a push webhook relay",
    }))
}

pub async fn tick_a2a_push_webhook_outbox_for_buffers(
    replay_buffers: &ReplayBufferMap,
) -> Result<(), OutboxRelayError> {
    let attachment = {
        let mut registry = registry().lock();
        prune(&mut registry);
        registry
            .get(&key(replay_buffers))
            .and_then(|attachment| attachment.a2a_push_relay.clone())
    };
    let Some(attachment) = attachment else {
        return Ok(());
    };
    let handler = Arc::new(A2aPushWebhookRelayHandler::new(reqwest::Client::new()));
    OutboxRelay::new(attachment.outbox, handler, attachment.config.relay)?
        .tick()
        .await
        .map(|_| ())
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
mod tests;
