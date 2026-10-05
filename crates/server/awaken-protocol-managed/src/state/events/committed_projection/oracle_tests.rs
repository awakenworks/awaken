//! The permanent warm/cold oracle over every published refresh.
//!
//! After this replica publishes a refresh, the oracle rehydrates a fresh
//! record from the durable Session root, reduces the same committed prefix
//! onto it with the one candidate builder, and compares the durable histories
//! (ids, payloads and completion times in order; process-local `evt_N`
//! overlays excluded). A published history that a cold replica of the same
//! prefix would not publish is a divergence: the timeline must be a function
//! of the committed prefix alone, never of which prefixes this replica
//! happened to refresh at.
//!
//! Consumer: every unit test of this crate that refreshes a Session, and the
//! ratchet below, which lets the known divergences only shrink.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use super::*;

/// Every test shares this one zero ceiling; no history or refresh schedule is
/// exempt from the warm/cold comparison except the two explicit read-counting
/// tests, whose own source-read budget would be changed by the oracle read.
const DIVERGENCE_CEILING: usize = 0;

/// Divergent publication points observed per test in this process.
static OBSERVED: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(Mutex::default);

type DurableEvent = (String, serde_json::Value, Option<String>);

fn durable_history(events: &[Event]) -> Vec<DurableEvent> {
    events
        .iter()
        .filter(|event| !is_transient_event_id(&event.id))
        .map(|event| {
            (
                event.id.clone(),
                serde_json::to_value(&event.kind).expect("Managed event kinds serialize"),
                event.processed_at.clone(),
            )
        })
        .collect()
}

/// A readable account of how two durable histories differ.
fn describe_divergence(
    warm: &[DurableEvent],
    cold: &[DurableEvent],
    warm_keys: &HashMap<String, awaken_managed_session_events::FactKey>,
    cold_keys: &HashMap<String, awaken_managed_session_events::FactKey>,
) -> String {
    let line = |(id, kind, _): &DurableEvent, keys: &HashMap<_, _>| {
        format!(
            "{id} {} {:?}",
            kind["type"].as_str().unwrap_or("?"),
            keys.get(id)
        )
    };
    let cold_ids = cold.iter().map(|event| &event.0).collect::<Vec<_>>();
    let warm_ids = warm.iter().map(|event| &event.0).collect::<Vec<_>>();
    let mut lines = Vec::new();
    for event in warm.iter().filter(|event| !cold_ids.contains(&&event.0)) {
        lines.push(format!("  warm only: {}", line(event, warm_keys)));
    }
    for event in cold.iter().filter(|event| !warm_ids.contains(&&event.0)) {
        lines.push(format!("  cold only: {}", line(event, cold_keys)));
    }
    for (warm_event, cold_event) in warm.iter().filter_map(|event| {
        cold.iter()
            .find(|candidate| candidate.0 == event.0)
            .map(|candidate| (event, candidate))
    }) {
        if warm_event != cold_event {
            lines.push(format!(
                "  same id, different payload or time: {}\n    warm {} {:?}\n    cold {} {:?}",
                warm_event.0, warm_event.1, warm_event.2, cold_event.1, cold_event.2
            ));
        }
    }
    if lines.is_empty() {
        lines.push("  same events, different order".into());
    }
    lines.push(format!(
        "  warm order: {:?}\n  cold order: {:?}",
        warm.iter()
            .map(|event| event.1["type"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>(),
        cold.iter()
            .map(|event| event.1["type"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>(),
    ));
    lines.join("\n")
}

impl ManagedState {
    /// Compare what this replica just published for `session_id` with what a
    /// cold replica builds from the same committed prefix. A divergence the
    /// running test is not allowed fails that test.
    pub(super) async fn check_warm_equals_cold_rebuild(&self, session_id: &str) {
        if !self
            .warm_cold_oracle
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let Some(warm) = self.sessions.lock().unwrap().get(session_id).cloned() else {
            return;
        };
        let cold = match self.cold_rebuild(session_id).await {
            Ok(Some(cold)) => cold,
            // The prefix moved between the publication and the rebuild, so
            // the two records describe different prefixes.
            Ok(None) => return,
            Err(error) => panic!(
                "warm/cold oracle: {session_id} published warm, but a cold rebuild of the same \
                 prefix fails: {error}"
            ),
        };
        if cold.checkpoint.source != warm.checkpoint.source {
            return;
        }
        let warm_history = durable_history(&warm.events);
        let cold_history = durable_history(&cold.events);
        if warm_history == cold_history {
            return;
        }
        let test = std::thread::current()
            .name()
            .unwrap_or("<unnamed thread>")
            .to_string();
        let observed = {
            let mut observed = OBSERVED.lock().unwrap();
            let count = observed.entry(test.clone()).or_default();
            *count += 1;
            *count
        };
        assert!(
            observed == DIVERGENCE_CEILING,
            "warm/cold oracle: divergent publication {observed} of {test} exceeds the zero \
             ceiling for Session {session_id}:\n{}",
            describe_divergence(
                &warm_history,
                &cold_history,
                &warm.checkpoint.progress.fact_keys,
                &cold.checkpoint.progress.fact_keys,
            )
        );
    }

    /// Rehydrate a record from the durable Session root exactly as a cold
    /// replica does, then reduce the current committed prefix onto it.
    async fn cold_rebuild(&self, session_id: &str) -> Result<Option<SessionRecord>, StateError> {
        let started = awaken_time::mono::Stopwatch::start();
        let recovered = self.read_persisted_session_projection(session_id).await?;
        let persisted = recovered.session;
        let base = self.committed_prefix_base(
            session_id,
            &recovered.owner_scope,
            &persisted,
            ProjectionDeliveryMemo::default(),
        )?;
        match self
            .build_committed_projection_candidate_over(session_id, persisted, base, started)
            .await?
        {
            publication::ProjectionRefreshBuild::Complete(candidate) => {
                Ok(Some(candidate.into_record()))
            }
            publication::ProjectionRefreshBuild::Retry => Ok(None),
        }
    }
}

#[test]
fn the_divergence_ceiling_cannot_regress_above_zero() {
    // Cause: a future edit tries to admit even one known divergence again.
    // Effect: this policy test fails before a green oracle result can bless it.
    // Rule Z1: every warm/cold comparison has a zero allowance.
    assert_eq!(DIVERGENCE_CEILING, 0, "Z1 no divergence exemptions");
}

#[tokio::test]
async fn candidate_uses_committed_prefix_even_when_warm_progress_is_poisoned() {
    // Causes: P1 one fixed Session/Thread committed prefix; P2 a clean cache
    // base or a base carrying phantom durable Event/child/owner, consumed
    // Message id and advanced lifecycle/budget cursors; P3 a local `evt_N`
    // overlay. Effects:
    // E1 P1+P2 yields the same durable ids/payload/order and checkpoint;
    // E2 P3 survives as delivery-only local state without changing E1.
    // Rules: R1=P1+clean=>E1; R2=P1+poisoned=>E1;
    // R3=P1+poisoned+P3=>E1+E2. The source fence and publication CAS are
    // exercised separately; this test pins the reducer's input boundary.
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::Id as RunId;

    let runtime = crate::state::test_support::RehydrateFake::default();
    let message = Message::text(
        MessageId::assistant(&RunId("prefix-run".into()), 0),
        Role::Assistant,
        "committed answer",
    );
    *runtime.committed.lock().unwrap() = Some(vec![message.clone()]);
    let state = ManagedState::new(runtime);
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder", "environment_id": "env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let session_id = session.id.as_str();
    let recovered = state
        .read_persisted_session_projection(session_id)
        .await
        .unwrap();
    let persisted = recovered.session;
    let owner_scope = recovered.owner_scope;
    let base = state
        .sessions
        .lock()
        .unwrap()
        .get(session_id)
        .cloned()
        .unwrap();
    let build = |base: SessionRecord| {
        let fresh = state
            .committed_prefix_base(
                session_id,
                &owner_scope,
                &persisted,
                base.into_delivery_memo(),
            )
            .unwrap();
        state.build_committed_projection_candidate_over(
            session_id,
            persisted.clone(),
            fresh,
            awaken_time::mono::Stopwatch::start(),
        )
    };
    let publication::ProjectionRefreshBuild::Complete(clean) = build(base.clone()).await.unwrap()
    else {
        panic!("R1 fixed committed prefix must project");
    };
    let clean = clean.into_record();

    let mut poisoned = base;
    poisoned.events.push(Event {
        id: "phantom-durable".into(),
        kind: OutboundKind::AgentThinking {},
        processed_at: None,
    });
    let phantom_agent = crate::types::session::SessionThreadAgent::from(&poisoned.session.agent);
    poisoned.child_threads.push(ManagedState::child_thread(
        &poisoned.session,
        "phantom-child",
        phantom_agent,
    ));
    poisoned
        .event_thread_owners
        .insert("phantom-durable".into(), "phantom-child".into());
    poisoned
        .checkpoint
        .progress
        .thread_message_ids
        .insert((session_id.to_string(), message.id.0));
    poisoned.checkpoint.source.lifecycle_cursor =
        awaken_agent_contract::RunLifecycleCursor(u64::MAX);
    poisoned.checkpoint.source.budget_reach_generation = u64::MAX;
    let overlay = Event {
        id: "evt_7".into(),
        kind: OutboundKind::AgentThinking {},
        processed_at: None,
    };
    poisoned.events.push(overlay.clone());
    poisoned.overlay.anchors.insert(
        overlay.id.clone(),
        clean.events.first().expect("R1 committed event").id.clone(),
    );
    let publication::ProjectionRefreshBuild::Complete(actual) = build(poisoned).await.unwrap()
    else {
        panic!("R2 fixed committed prefix must project");
    };
    let actual = actual.into_record();
    assert_eq!(
        durable_history(&actual.events),
        durable_history(&clean.events),
        "R1-R3/E1"
    );
    assert!(
        actual.checkpoint == clean.checkpoint,
        "R1-R3/E1 source and progress"
    );
    assert_eq!(
        serde_json::to_value(&actual.child_threads).unwrap(),
        serde_json::to_value(&clean.child_threads).unwrap(),
        "R1-R3/E1 children are committed-derived"
    );
    assert!(
        actual.events.iter().any(|event| event.id == overlay.id),
        "R3/E2 overlay retained"
    );
}
