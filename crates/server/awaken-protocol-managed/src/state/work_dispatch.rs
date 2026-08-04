//! Durable Session-to-WorkQueue projection reconciliation.

use super::ManagedState;

impl ManagedState {
    /// Restore the WorkQueue projection for every durable, externally executed
    /// Session. The frozen baseline is the sole placement authority; this scan
    /// never reopens the mutable Environment catalog. `enqueue_session` is an
    /// idempotent projection write, so startup, periodic recovery, and the
    /// create fast path may race or replay without duplicating execution work.
    pub async fn reconcile_work_dispatches(&self) -> usize {
        let sessions = self.sessions_repo.reconcilable_sessions().await;
        let mut settled = 0;
        for scoped in sessions {
            let session = scoped.session;
            if !session.needs_work_dispatch() {
                continue;
            }
            let Some(baseline) = session.frozen_baseline() else {
                continue;
            };
            match self
                .environments
                .enqueue_session_work(&baseline.environment.environment_id, &session.session_id)
                .await
            {
                Ok(_) => settled += 1,
                Err(error) => tracing::warn!(
                    session = %session.session_id,
                    environment = %baseline.environment.environment_id,
                    error = ?error,
                    "Session WorkQueue dispatch remains pending"
                ),
            }
        }
        settled
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_session_contract::ManagedSessionRepository;
    use awaken_session_contract::work_queue::WorkQueue;

    use super::*;

    fn persisted(
        id: &str,
        self_hosted: bool,
        application: bool,
        status: &str,
    ) -> awaken_session_contract::PersistedSession {
        let environment = awaken_session_contract::EnvironmentSnapshot {
            environment_id: "env-worker".into(),
            revision: awaken_environment_contract::EnvironmentRevision(7),
            self_hosted,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-7".into()),
            sandbox: serde_json::json!({}),
            sandbox_provisioning: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        awaken_session_contract::PersistedSession {
            session_id: id.into(),
            revision: Default::default(),
            baseline: awaken_session_contract::SessionBaselineState::Frozen(
                awaken_session_contract::SessionBaseline::compile(
                    awaken_session_contract::SessionBaselineInputs {
                        environment,
                        mcp_authoring: Default::default(),
                        agent_id: "agent".into(),
                        model: "model".into(),
                        runtime: None,
                        application: application.then(|| {
                            awaken_session_contract::ApplicationContributionReceipt {
                                plan_fingerprint: "plan".into(),
                                input_fingerprint: "input".into(),
                            }
                        }),
                        delegate_ids: Vec::new(),
                        toolsets: Vec::new(),
                        mounts: Vec::new(),
                        env: Vec::new(),
                        prompts: Vec::new(),
                    },
                ),
            ),
            title: None,
            metadata: Default::default(),
            tools: Default::default(),
            activity_epoch: 0,
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            status: status.into(),
            archived_at: None,
        }
    }

    async fn create(
        repo: &dyn ManagedSessionRepository,
        session: awaken_session_contract::PersistedSession,
    ) {
        let id = session.session_id.clone();
        repo.create(
            "workspace",
            session,
            awaken_session_contract::IdempotencyRecord {
                key: format!("create:{id}"),
                payload_hash: format!("payload:{id}"),
            },
            Vec::new(),
        )
        .await
        .expect("persist fixture");
    }

    #[tokio::test]
    async fn durable_session_truth_recovers_one_missing_work_projection() {
        // Cause/effect graph: C1 frozen Environment is self-hosted; C2 Session
        // has no Application-owned execution; C3 Session is nonterminal; C4 the
        // queue projection is missing or already present. Effects: E1 only
        // C1+C2+C3 is dispatched; E2 C4 missing creates one item; E3 C4 present
        // replays the same identity without duplication. Terminal, local, and
        // Application Sessions must never be projected by this coordinator.
        //
        // | Rule | self-hosted | application | terminal | projection | effect |
        // | R1 | yes | no | no | missing | create one |
        // | R2 | yes | no | no | present | retain one |
        // | R3 | no | no | no | any | skip |
        // | R4 | yes | yes | no | any | skip |
        // | R5 | yes | no | yes | any | skip |
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        create(repo.as_ref(), persisted("external", true, false, "idle")).await;
        create(repo.as_ref(), persisted("local", false, false, "idle")).await;
        create(repo.as_ref(), persisted("application", true, true, "idle")).await;
        create(
            repo.as_ref(),
            persisted("terminal", true, false, "terminated"),
        )
        .await;
        assert_eq!(
            repo.reconcilable_sessions().await.len(),
            1,
            "only R1/R2 belongs to the canonical reconciliation scan"
        );
        assert!(
            repo.get("external")
                .await
                .and_then(|session| session.frozen_baseline().cloned())
                .is_some_and(|baseline| baseline.environment.self_hosted),
            "external placement is durable"
        );

        let work = Arc::new(awaken_work_store::InMemoryWorkQueue::new());
        let environments = Arc::new(crate::routes::environments::EnvironmentExecutionState::new(
            work.clone(),
            Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new()),
        ));
        let state = ManagedState::new(crate::state::tests::RehydrateFake::default())
            .with_session_repo(repo)
            .with_environments(environments);

        assert_eq!(state.reconcile_work_dispatches().await, 1, "R1");
        let first = work.list("env-worker").await.expect("R1 list");
        assert_eq!(first.len(), 1, "R1/R3/R4/R5");
        assert!(matches!(
            &first[0].data,
            awaken_session_contract::work_queue::WorkPayload::Session { id } if id == "external"
        ));

        assert_eq!(state.reconcile_work_dispatches().await, 1, "R2");
        let replay = work.list("env-worker").await.expect("R2 list");
        assert_eq!(replay.len(), 1, "R2");
        assert_eq!(replay[0].id, first[0].id, "R2 canonical identity");
    }
}
