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

/// Project the result of the authoritative claim. A Session token is wrapped in
/// the SDK's base64url `BetaWorkSecret` envelope at this wire boundary and is
/// never copied into the durable Work item or subsequent retrieve/list views.
#[must_use]
pub(crate) fn project_claim(claim: &ClaimedWork) -> Work {
    use base64::Engine as _;

    let secret = claim.sessions_token.as_ref().map(|token| {
        let payload = serde_json::json!({
            "sessions_token": token.expose_secret(),
        });
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).expect("Work secret envelope serializes"))
    });
    project_work_with_secret(&claim.item, secret)
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::collections::BTreeMap;

    fn item(data: WorkPayload) -> WorkItem {
        WorkItem {
            id: "work_1".into(),
            environment_id: "env_1".into(),
            data,
            metadata: BTreeMap::new(),
            state: WorkState::Active,
            acknowledged_at: None,
            latest_heartbeat_at: None,
            started_at: None,
            stop_requested_at: None,
            stopped_at: None,
        }
    }

    #[test]
    fn only_session_claim_projects_the_official_one_time_secret_envelope() {
        // Cause/effect graph: C1 ordinary list/retrieve projection; C2 claimed
        // HealthCheck; C3 claimed Session with a sessions token. Effects: E1/E2
        // secret is null; E3 secret is base64url JSON containing exactly the
        // sessions_token. Constraints: K1 cleartext exists only at this claim
        // projection seam; K2 the neutral WorkItem remains secret-free.
        // Decision rows W1=C1->E1, W2=C2->E2, W3=C3->E3.
        let session = item(WorkPayload::Session {
            id: "sesn_1".into(),
        });
        assert!(project_work(&session).secret.0.is_none(), "W1");
        assert!(
            project_claim(&ClaimedWork {
                item: item(WorkPayload::HealthCheck {
                    id: "work_health".into(),
                }),
                sessions_token: None,
            })
            .secret
            .0
            .is_none(),
            "W2"
        );
        let wire = project_claim(&ClaimedWork {
            item: session,
            sessions_token: Some(awaken_agent_contract::RedactedString::from(
                "sk-ant-req-example".to_string(),
            )),
        });
        let encoded = wire.secret.0.expect("W3 envelope");
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .expect("W3 base64url");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&decoded).unwrap(),
            serde_json::json!({"sessions_token": "sk-ant-req-example"}),
            "W3"
        );
    }
}
