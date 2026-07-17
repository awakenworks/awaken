//! The environment work-queue port now lives in `awaken-session-contract` (a
//! contract/ leaf); this module re-exports it and owns the neutral→wire projection
//! (`WorkItem` → the Anthropic `BetaSelfHostedWork`), the one place the wire shape is
//! named. Re-exported here so existing `awaken_protocol_managed::work_queue::…` paths
//! keep resolving until consumers flip to the contract directly.

pub use awaken_session_contract::work_queue::*;

use crate::types::environment::{Work, WorkData};

/// Project a neutral [`WorkItem`] to the official `BetaSelfHostedWork` wire shape.
/// `secret` is always `null` (no per-lease token is minted here).
#[must_use]
pub(crate) fn project_work(item: &WorkItem) -> Work {
    Work {
        id: item.id.clone(),
        object_type: "work",
        environment_id: item.environment_id.clone(),
        data: project_payload(&item.data),
        metadata: item.metadata.clone(),
        state: item.state.as_str(),
        secret: None,
        acknowledged_at: item.acknowledged_at.clone(),
        latest_heartbeat_at: item.latest_heartbeat_at.clone(),
        created_at: OBJECT_AT.to_string(),
        started_at: item.started_at.clone(),
        stop_requested_at: item.stop_requested_at.clone(),
        stopped_at: item.stopped_at.clone(),
    }
}

/// Map the neutral work payload onto the tagged `BetaSelfHostedWork.data` wire enum.
fn project_payload(data: &WorkPayload) -> WorkData {
    match data {
        WorkPayload::HealthCheck { id } => WorkData::HealthCheck { id: id.clone() },
        WorkPayload::Session { id } => WorkData::Session { id: id.clone() },
    }
}
