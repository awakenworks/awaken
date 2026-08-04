//! Root-CAS commands for Session activity. Public wire status remains the sole
//! lifecycle fact; the scalar epoch only fences stale overlapping completions.

use super::*;

impl ManagedState {
    pub(crate) async fn begin_session_activity(&self, session_id: &str) -> Result<u64, StateError> {
        for attempt in 0..awaken_session_application::SessionApplication::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .application
                .session_repository()
                .owner(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            let mut session = self
                .application
                .session_repository()
                .get(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            if session.is_terminal() {
                return Err(StateError::Archived);
            }
            session.activity_epoch = session.activity_epoch.saturating_add(1);
            session.status = "running".into();
            let epoch = session.activity_epoch;
            match self
                .commit_session_snapshot(&owner_scope, session, "begin-activity", Vec::new())
                .await
            {
                Ok(_) => return Ok(epoch),
                Err(StateError::Conflict)
                    if attempt + 1
                        < awaken_session_application::SessionApplication::ROOT_CAS_ATTEMPTS =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(StateError::Conflict)
    }

    pub(crate) async fn settle_session_activity(
        &self,
        session_id: &str,
        expected_epoch: u64,
    ) -> Result<(), StateError> {
        for attempt in 0..awaken_session_application::SessionApplication::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .application
                .session_repository()
                .owner(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            let mut session = self
                .application
                .session_repository()
                .get(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            // A later event already owns the Session. The stale completion may
            // project its transcript, but it must not make the newer turn idle.
            if session.activity_epoch != expected_epoch {
                return Ok(());
            }
            session.status = "idle".into();
            match self
                .commit_session_snapshot(&owner_scope, session, "settle-activity", Vec::new())
                .await
            {
                Ok(_) => return Ok(()),
                Err(StateError::Conflict)
                    if attempt + 1
                        < awaken_session_application::SessionApplication::ROOT_CAS_ATTEMPTS =>
                {
                    continue;
                }
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
    /// E1=admission increments epoch and marks running; E2=stale settlement is a
    /// no-op; E3=current settlement marks idle. Rules:
    /// A1 C1=>E1; A2 C1+C2+!C3=>E2; A3 C1+C3=>E3.
    #[tokio::test]
    async fn epoch_prevents_a_stale_turn_from_idling_a_newer_turn() {
        let state = ManagedState::new(crate::state::tests::RehydrateFake::default());
        let request = serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap();
        let session = state.create_session(request, None).await.unwrap();

        let first = state.begin_session_activity(&session.id).await.unwrap();
        let second = state.begin_session_activity(&session.id).await.unwrap();
        assert_eq!(second, first + 1);
        let active = state
            .application
            .session_repository()
            .get(&session.id)
            .await
            .unwrap();
        assert_eq!(active.status, "running");
        assert_eq!(active.activity_epoch, second);

        state
            .settle_session_activity(&session.id, first)
            .await
            .unwrap();
        let still_active = state
            .application
            .session_repository()
            .get(&session.id)
            .await
            .unwrap();
        assert_eq!(still_active.activity_epoch, active.activity_epoch);
        assert_eq!(still_active.status, "running");

        state
            .settle_session_activity(&session.id, second)
            .await
            .unwrap();
        let idle = state
            .application
            .session_repository()
            .get(&session.id)
            .await
            .unwrap();
        assert_eq!(idle.status, "idle");
        assert_eq!(idle.activity_epoch, second);
    }
}
