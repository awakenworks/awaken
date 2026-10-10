//! The existing profiled release route's one command-lowering seam.
//!
//! Route observations describe the winning candidate, never command identity.
//! The Session Application compiles its idempotency record from stable intent.

use crate::state::lifecycle_fact;

pub(super) fn command(
    owner_scope: &str,
    id: &str,
    publication: crate::awaken_extensions::ProfiledSessionRepositoryPublication,
    observed_at: u64,
) -> awaken_session_application::SessionArchiveWithRepositoryPublicationCommand {
    awaken_session_application::SessionArchiveWithRepositoryPublicationCommand {
        owner_scope: owner_scope.to_owned(),
        session_id: awaken_session_types::SessionId::from(id),
        archived_at: awaken_session_application_contract::epoch_millis_to_rfc3339(observed_at),
        lifecycle_fact: lifecycle_fact(
            awaken_session_event_contract::session_terminated_fact_id(id),
            id,
            Some(owner_scope.to_owned()),
            awaken_session_event_contract::SESSION_TERMINATED,
            observed_at,
        ),
        repository: awaken_session_application::SessionRepositoryPublicationSelector {
            binding_id: awaken_resource_contract::BindingId::new(publication.binding_id),
            expectation: publication.expectation,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_route_requests_keep_intent_while_observation_time_changes() {
        // Causes: P1 two HTTP releases carry the same Repository selector but
        // the Route clock samples 1s and 2s; P2 another request changes the
        // expected Git commit. Effects: P1 identical Session/Workspace/fact
        // identity and selector, different display/fact timestamps; P2 a
        // distinct business intent. The Application lost-CAS table checks that
        // P1 converges on one idempotency receipt, archive time and effect.
        let publication: crate::awaken_extensions::ProfiledSessionRepositoryPublication =
            serde_json::from_value(serde_json::json!({
                "binding_id": "source",
                "expectation": {
                    "branch": "awf/work",
                    "commit": "0123456789abcdef0123456789abcdef01234567"
                }
            }))
            .unwrap();
        let first = command("workspace", "session", publication.clone(), 1_000);
        let second = command("workspace", "session", publication.clone(), 2_000);
        assert_eq!(first.owner_scope, second.owner_scope, "P1");
        assert_eq!(first.session_id, second.session_id, "P1");
        assert_eq!(first.lifecycle_fact.id, second.lifecycle_fact.id, "P1");
        assert_eq!(
            first.lifecycle_fact.event_type, second.lifecycle_fact.event_type,
            "P1"
        );
        assert_eq!(first.repository, second.repository, "P1");
        assert_ne!(first.archived_at, second.archived_at, "P1");
        assert_ne!(
            first.lifecycle_fact.timestamp, second.lifecycle_fact.timestamp,
            "P1"
        );
        let mut changed = publication;
        changed.expectation.commit = "1111111111111111111111111111111111111111".into();
        assert_ne!(
            first.repository,
            command("workspace", "session", changed, 3_000).repository,
            "P2"
        );
    }
}
