//! Root-CAS commands for Session activity. Public wire events remain projections;
//! this durable fence exists so an idle-environment reconciler cannot race a new turn.

use super::*;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

impl ManagedState {
    pub(crate) async fn begin_session_activity(&self, session_id: &str) -> Result<u64, StateError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .sessions_repo
                .owner(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            let mut session = self
                .sessions_repo
                .get(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            if session.is_terminal() {
                return Err(StateError::Archived);
            }
            session.activity.epoch = session.activity.epoch.saturating_add(1);
            session.activity.state = awaken_session_contract::SessionActivityState::Active;
            session.status = "running".into();
            let epoch = session.activity.epoch;
            match self
                .commit_session_snapshot(&owner_scope, session, "begin-activity", Vec::new())
                .await
            {
                Ok(_) => return Ok(epoch),
                Err(StateError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => continue,
                Err(error) => return Err(error),
            }
        }
        Err(StateError::Conflict)
    }

    pub(crate) async fn settle_session_activity(
        &self,
        session_id: &str,
        expected_epoch: u64,
        reason: awaken_session_contract::SessionIdleReason,
    ) -> Result<(), StateError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .sessions_repo
                .owner(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            let mut session = self
                .sessions_repo
                .get(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            // A later event already owns the Session. The stale completion may
            // project its transcript, but it must not make the newer turn idle.
            if session.activity.epoch != expected_epoch {
                return Ok(());
            }
            session.activity.state = awaken_session_contract::SessionActivityState::Idle {
                reason: reason.clone(),
                since_unix_ms: now_unix_ms(),
            };
            session.status = "idle".into();
            match self
                .commit_session_snapshot(&owner_scope, session, "settle-activity", Vec::new())
                .await
            {
                Ok(_) => return Ok(()),
                Err(StateError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => continue,
                Err(error) => return Err(error),
            }
        }
        Err(StateError::Conflict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Activity-fence cause/effect graph and decision table.
    /// C1=admit a driving event; C2=a later driving event is admitted before the
    /// earlier one settles; C3=settlement epoch matches current root epoch.
    /// E1=admission increments epoch and marks Active/running; E2=stale settlement
    /// is a no-op; E3=current settlement marks Idle/end-turn. Rules:
    /// A1 C1=>E1; A2 C1+C2+!C3=>E2; A3 C1+C3=>E3.
    #[tokio::test]
    async fn epoch_prevents_a_stale_turn_from_idling_a_newer_turn() {
        let state = ManagedState::new(crate::state::tests::RehydrateFake::default());
        let request = serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap();
        let session = state.create_session(request, None).await.unwrap();

        let first = state.begin_session_activity(&session.id).await.unwrap();
        let second = state.begin_session_activity(&session.id).await.unwrap();
        assert_eq!(second, first + 1);
        let active = state.sessions_repo.get(&session.id).await.unwrap();
        assert_eq!(active.status, "running");
        assert_eq!(active.activity.epoch, second);
        assert_eq!(
            active.activity.state,
            awaken_session_contract::SessionActivityState::Active
        );

        state
            .settle_session_activity(
                &session.id,
                first,
                awaken_session_contract::SessionIdleReason::EndTurn,
            )
            .await
            .unwrap();
        let still_active = state.sessions_repo.get(&session.id).await.unwrap();
        assert_eq!(still_active.activity, active.activity);

        state
            .settle_session_activity(
                &session.id,
                second,
                awaken_session_contract::SessionIdleReason::EndTurn,
            )
            .await
            .unwrap();
        let idle = state.sessions_repo.get(&session.id).await.unwrap();
        assert_eq!(idle.status, "idle");
        assert!(matches!(
            idle.activity.state,
            awaken_session_contract::SessionActivityState::Idle {
                reason: awaken_session_contract::SessionIdleReason::EndTurn,
                ..
            }
        ));
    }
}
