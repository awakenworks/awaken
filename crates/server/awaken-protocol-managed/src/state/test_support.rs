// @formal surface exclude=test_only
// @formal owner crates/server/awaken-protocol-managed/src/state/tests/mod.rs
// @formal reason: Managed-state fixtures and fake runtime compiled only through the test-gated parent
// @formal reason: module.
//! Shared Managed-state test fixtures.
//!
//! These fixtures serve rehydration, deployment-session, and activity tests.
//! Keeping the single fake Runtime here avoids parallel fake implementations in
//! those behavior modules while leaving each test beside the behavior it covers.

pub(crate) use awaken_protocol_managed_testkit::{
    ReadControl, RehydrateFake, create_session_fixture, ephemeral_session_repo,
};
pub(crate) use session_repository::ConflictInjectingRepo;

pub(crate) fn managed_state_from_parts(
    runtime: std::sync::Arc<dyn awaken_session_execution_contract::CompleteSessionRuntime>,
    mcp: Option<std::sync::Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>>,
    sessions: std::sync::Arc<dyn awaken_session_application::SessionStore>,
    environments: std::sync::Arc<
        awaken_environment_application::execution::EnvironmentExecutionApplication,
    >,
    clock: std::sync::Arc<dyn awaken_time::WallClock>,
    compiler: std::sync::Arc<dyn awaken_tool_contract::ToolConfigurationCompiler>,
) -> crate::ManagedState {
    crate::ManagedState::from_application(
        awaken_protocol_managed_testkit::session_application_from_parts(
            awaken_session_execution_contract::SessionExecutor::colocated(
                runtime,
                mcp.unwrap_or_else(|| {
                    std::sync::Arc::new(awaken_session_execution_contract::NeutralRuntime)
                }),
            ),
            sessions,
            environments.clone(),
            clock.clone(),
            {
                let probe = std::sync::Arc::new(
                    awaken_session_application::ExecutionObservationProbe::default(),
                );
                awaken_session_application::SessionExecutionAdmission::new(
                    std::sync::Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone())),
                    probe.clone(),
                    probe,
                )
            },
            compiler,
        ),
        awaken_data_governance_contract::CapturePolicy::default(),
    )
}

#[cfg(test)]
mod session_repository {
    //! Existing real-SQLite conflict fixture, shared with controlled refresh reads.
    //! Every operation still delegates to SQLite except the explicitly armed CAS
    //! conflict or read failure. This is the sole repository decorator for both.

    use async_trait::async_trait;
    use awaken_protocol_managed_testkit::ReadControl;
    use awaken_session_contract::PersistedSession;
    use awaken_session_contract::SessionRepository;
    use awaken_session_store::SqliteManagedSessionRepository;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    pub(crate) struct ConflictInjectingRepo {
        inner: Arc<SqliteManagedSessionRepository>,
        conflicts: AtomicU64,
        reads: ReadControl,
    }

    impl ConflictInjectingRepo {
        pub(crate) fn new(
            inner: Arc<SqliteManagedSessionRepository>,
            conflicts: u64,
            reads: ReadControl,
        ) -> Self {
            Self {
                inner,
                conflicts: AtomicU64::new(conflicts),
                reads,
            }
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRepositoryReads for ConflictInjectingRepo {
        async fn get(
            &self,
            session_id: &awaken_session_types::SessionId,
        ) -> Result<PersistedSession, awaken_session_contract::SessionRepositoryError> {
            self.reads.on_read("session").await.map_err(|port| {
                awaken_session_contract::SessionRepositoryError::AuthorityUnavailable(format!(
                    "scripted {port} read failure"
                ))
            })?;
            self.inner.get(session_id).await
        }

        async fn owner(
            &self,
            session_id: &awaken_session_types::SessionId,
        ) -> Result<String, awaken_session_contract::SessionRepositoryError> {
            self.inner.owner(session_id).await
        }

        async fn scan(
            &self,
            query: awaken_session_contract::SessionScanQuery<'_>,
        ) -> Result<
            awaken_session_contract::SessionScan,
            awaken_session_contract::SessionRepositoryError,
        > {
            self.inner.scan(query).await
        }

        async fn deleted_event_admissions(
            &self,
            owner_scope: &str,
            session_id: &awaken_session_types::SessionId,
        ) -> Result<
            Option<awaken_session_contract::DeletedSessionEventAdmissions>,
            awaken_session_contract::SessionRepositoryError,
        > {
            self.inner
                .deleted_event_admissions(owner_scope, session_id)
                .await
        }
    }

    #[async_trait]
    impl SessionRepository for ConflictInjectingRepo {
        async fn create(
            &self,
            owner_scope: &str,
            session: PersistedSession,
            idempotency: awaken_session_contract::IdempotencyRecord,
            facts: Vec<awaken_session_contract::ManagedLifecycleFact>,
        ) -> Result<
            awaken_session_contract::SessionCreateResult,
            awaken_session_contract::SessionRepositoryError,
        > {
            self.inner
                .create(owner_scope, session, idempotency, facts)
                .await
        }

        async fn replay_create(
            &self,
            owner_scope: &str,
            session_id: &awaken_session_types::SessionId,
            idempotency: &awaken_session_contract::IdempotencyRecord,
        ) -> Result<Option<PersistedSession>, awaken_session_contract::SessionRepositoryError>
        {
            self.inner
                .replay_create(owner_scope, session_id, idempotency)
                .await
        }

        async fn commit(
            &self,
            owner_scope: &str,
            mutation: awaken_session_contract::SessionMutation,
        ) -> Result<
            awaken_session_contract::SessionMutationResult,
            awaken_session_contract::SessionRepositoryError,
        > {
            if self
                .conflicts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    count.checked_sub(1)
                })
                .is_ok()
            {
                return Ok(awaken_session_contract::SessionMutationResult::Conflict {
                    current_revision: mutation.expected_revision,
                });
            }
            self.inner.commit(owner_scope, mutation).await
        }

        async fn idempotency_receipt(
            &self,
            session_id: &awaken_session_types::SessionId,
            key: &str,
        ) -> Result<
            Option<awaken_session_contract::SessionIdempotencyReceipt>,
            awaken_session_contract::SessionRepositoryError,
        > {
            self.inner.idempotency_receipt(session_id, key).await
        }

        async fn count_environment_phase(
            &self,
            phase: awaken_session_contract::SessionEnvironmentPhase,
        ) -> Result<u64, awaken_session_contract::SessionRepositoryError> {
            self.inner.count_environment_phase(phase).await
        }
    }

    use awaken_session_contract::SessionReconciliation;

    #[async_trait::async_trait]
    impl SessionReconciliation for ConflictInjectingRepo {
        async fn reconciliation_scan(
            &self,
            after: Option<&awaken_session_contract::SessionScanCursor>,
        ) -> Result<
            awaken_session_contract::SessionScan,
            awaken_session_contract::SessionRepositoryError,
        > {
            self.inner.reconciliation_scan(after).await
        }
    }
    #[async_trait::async_trait]
    impl awaken_session_contract::LifecycleOutbox for ConflictInjectingRepo {
        async fn append_lifecycle(
            &self,
            fact: awaken_session_contract::ManagedLifecycleFact,
        ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
            self.inner.append_lifecycle(fact).await
        }

        async fn pending_lifecycle(
            &self,
        ) -> Result<
            Vec<awaken_session_contract::ManagedLifecycleFact>,
            awaken_session_contract::SessionRepositoryError,
        > {
            self.inner.pending_lifecycle().await
        }

        async fn complete_lifecycle(
            &self,
            fact_id: &str,
        ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
            self.inner.complete_lifecycle(fact_id).await
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::PreparedParticipantIntents for ConflictInjectingRepo {
        async fn record_creation_intent(
            &self,
            owner_scope: &str,
            session_id: &awaken_session_types::SessionId,
            repository_id: &str,
            lease_expires_at_unix_ms: u64,
        ) -> Result<
            awaken_session_contract::PreparedParticipantIntentRecord,
            awaken_session_contract::SessionRepositoryError,
        > {
            self.inner
                .record_creation_intent(
                    owner_scope,
                    session_id,
                    repository_id,
                    lease_expires_at_unix_ms,
                )
                .await
        }

        async fn claim_creation_abort(
            &self,
            session_id: &awaken_session_types::SessionId,
        ) -> Result<
            awaken_session_contract::PreparedParticipantIntentClaim,
            awaken_session_contract::SessionRepositoryError,
        > {
            self.inner.claim_creation_abort(session_id).await
        }

        async fn complete_creation_abort(
            &self,
            session_id: &awaken_session_types::SessionId,
        ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
            self.inner.complete_creation_abort(session_id).await
        }

        async fn recoverable_creation_intents(
            &self,
            now_unix_ms: u64,
            limit: usize,
        ) -> Result<
            Vec<awaken_session_contract::PreparedParticipantIntent>,
            awaken_session_contract::SessionRepositoryError,
        > {
            self.inner
                .recoverable_creation_intents(now_unix_ms, limit)
                .await
        }
    }
}
