//! The environment work-queue port now lives in `awaken-session-contract` (a
//! contract/ leaf); this module consumes it and owns the neutral→wire projection
//! (`WorkItem` → the Anthropic `BetaSelfHostedWork`), the one place the wire shape is
//! named.

use awaken_session_contract::work_queue::WorkState as DomainWorkState;
pub(crate) use awaken_session_contract::work_queue::*;

use crate::types::environment::{Work, WorkData, WorkObjectType, WorkState as WireWorkState};

/// Project a neutral [`WorkItem`] to the official `BetaSelfHostedWork` wire shape.
/// `secret` is always `null` (no per-lease token is minted here).
#[must_use]
pub(crate) fn project_work(item: &WorkItem) -> Work {
    project_work_with_secret(item, None)
}

/// Poll-only projection. Retrieval/list/update never receive a secret; this
/// function makes that visibility boundary explicit at the single projector.
#[must_use]
pub(crate) fn project_work_with_secret(item: &WorkItem, secret: Option<String>) -> Work {
    Work {
        id: item.id.clone(),
        object_type: WorkObjectType::Work,
        environment_id: item.environment_id.clone(),
        data: project_payload(&item.data),
        metadata: item.metadata.clone(),
        state: project_state(item.state),
        secret: secret.into(),
        acknowledged_at: item.acknowledged_at.clone().into(),
        latest_heartbeat_at: item.latest_heartbeat_at.clone().into(),
        created_at: OBJECT_AT.to_string(),
        started_at: item.started_at.clone().into(),
        stop_requested_at: item.stop_requested_at.clone().into(),
        stopped_at: item.stopped_at.clone().into(),
    }
}

pub(crate) const fn project_state(state: DomainWorkState) -> WireWorkState {
    match state {
        DomainWorkState::Queued => WireWorkState::Queued,
        DomainWorkState::Starting => WireWorkState::Starting,
        DomainWorkState::Active => WireWorkState::Active,
        DomainWorkState::Stopping => WireWorkState::Stopping,
        DomainWorkState::Stopped => WireWorkState::Stopped,
    }
}

/// Map the neutral work payload onto the tagged `BetaSelfHostedWork.data` wire enum.
fn project_payload(data: &WorkPayload) -> WorkData {
    match data {
        WorkPayload::HealthCheck { id } => WorkData::HealthCheck { id: id.clone() },
        WorkPayload::Session { id } => WorkData::Session { id: id.clone() },
    }
}
