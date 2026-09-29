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
use crate::state::sessions::RehydrationPurpose;

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
        let started = awaken_lease_contract::mono::Stopwatch::start();
        let recovered = self.read_persisted_session_projection(session_id).await?;
        let persisted = recovered.session;
        let agent_id = persisted
            .agent_id()
            .map(str::to_string)
            .ok_or(StateError::NotFound)?;
        let session = self.rehydrated_session_for(
            session_id,
            &recovered.owner_scope,
            persisted.clone(),
            RehydrationPurpose::CollectionRead,
        )?;
        let base = SessionRecord::new(
            agent_id,
            session,
            persisted.revision,
            persisted.resources.clone(),
            Vec::new(),
        );
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
