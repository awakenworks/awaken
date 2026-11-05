//! The same fault-injection reader over its original selected repository.
use super::*;

#[async_trait::async_trait]
impl awaken_session_contract::SessionRepositoryReads for FaultingSessionRepository {
    async fn workspace_content_is_public(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<bool, awaken_session_contract::SessionRepositoryError> {
        self.inner
            .workspace_content_is_public(owner_scope, session_id)
            .await
    }

    async fn get(
        &self,
        session_id: &SessionId,
    ) -> Result<PersistedSession, awaken_session_contract::SessionRepositoryError> {
        let get_call = {
            let mut observed = self.observed_gets.lock().unwrap();
            observed.push(session_id.to_string());
            observed.len()
        };
        self.get_entered.notify_one();
        let gate = self
            .paused_gets
            .lock()
            .unwrap()
            .get(session_id.as_str())
            .cloned();
        if let Some(gate) = gate {
            gate.acquire()
                .await
                .expect("test read gate remains open")
                .forget();
        }
        if self
            .failed_gets
            .lock()
            .unwrap()
            .contains(session_id.as_str())
            || self
                .fail_get_on_call_once
                .compare_exchange(get_call, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Err(
                awaken_session_contract::SessionRepositoryError::AuthorityUnavailable(
                    "injected Session read failure".into(),
                ),
            );
        }
        if let Some(session) = self.get_override_once.lock().unwrap().take() {
            return Ok(session);
        }
        if self.get_not_found_once.swap(false, Ordering::SeqCst) {
            return Err(awaken_session_contract::SessionRepositoryError::NotFound);
        }
        let result = self.inner.get(session_id).await;
        self.completed_gets
            .lock()
            .unwrap()
            .push(session_id.to_string());
        self.get_entered.notify_one();
        result
    }

    async fn owner(
        &self,
        session_id: &SessionId,
    ) -> Result<String, awaken_session_contract::SessionRepositoryError> {
        self.inner.owner(session_id).await
    }

    async fn scan(
        &self,
        query: awaken_session_contract::SessionScanQuery<'_>,
    ) -> Result<awaken_session_contract::SessionScan, awaken_session_contract::SessionRepositoryError>
    {
        self.inner.scan(query).await
    }

    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<
        Option<awaken_session_contract::DeletedSessionEventAdmissions>,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner
            .deleted_event_admissions(owner_scope, session_id)
            .await
    }
}
