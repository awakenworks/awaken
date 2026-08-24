//! Historical Awaiting targets recovered from the Thread audit prefix.

use super::*;

pub(super) fn historical_pending_by_lifecycle<'a>(
    lifecycle_events: &[RunLifecycleEvent],
    snapshots: impl Fn(
        &str,
    )
        -> Option<&'a awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
) -> Result<std::collections::HashMap<awaken_agent_contract::RunLifecycleCursor, Pending>, StateError>
{
    let mut pending = std::collections::HashMap::new();
    let mut awaiting_ordinal = std::collections::HashMap::<(String, String), usize>::new();
    for lifecycle in lifecycle_events
        .iter()
        .filter(|event| event.kind == RunLifecycleEventKind::Awaiting)
    {
        let key = (lifecycle.thread_id.0.clone(), lifecycle.run_id.0.clone());
        let ordinal = awaiting_ordinal.entry(key).or_default();
        let historical = snapshots(&lifecycle.thread_id.0)
            .into_iter()
            .flat_map(|snapshot| snapshot.events.iter())
            .filter(|audit| {
                audit.run_id == lifecycle.run_id
                    && audit.kind == awaken_agent_contract::audit::kind::Kind::RunStateChanged
                    && serde_json::from_value::<awaken_agent_contract::agent::run::RunState>(
                        audit.payload["state"].clone(),
                    )
                    .is_ok_and(|state| {
                        state == awaken_agent_contract::agent::run::RunState::Awaiting
                    })
            })
            .nth(*ordinal)
            .and_then(|audit| audit.payload.get("await_target"))
            .map(|value| {
                serde_json::from_value::<awaken_agent_contract::agent::awaiting::AwaitTarget>(
                    value.clone(),
                )
            })
            .transpose()
            .map_err(|error| {
                StateError::Run(RunError::internal(format!(
                    "decode committed Awaiting target: {error}"
                )))
            })?
            .as_ref()
            .and_then(Pending::from_await_target);
        if let Some(historical) = historical {
            pending.insert(lifecycle.cursor, historical);
        }
        *ordinal += 1;
    }
    Ok(pending)
}
