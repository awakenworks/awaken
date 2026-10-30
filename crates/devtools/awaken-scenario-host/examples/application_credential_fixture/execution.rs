//! Qualification composition: real SQLite Session repository and the original
//! Dispatch claim/physical-slot adapter. No model or third-party account runs.
use awaken_credential_contract::{CredentialExecutionAuthority, CredentialExecutionBinding};
use awaken_dispatch::MemoryDispatchStore;
use awaken_dispatch::{
    AttemptExecutionBinding, DispatchAdministration, DispatchClaims, DispatchEnqueue,
    DispatchExecution, PlacementRequirements, RunClaim, RunDispatch, WorkerIdentity,
};
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_session_configuration_contract::{
    EnvironmentFingerprint, EnvironmentSnapshot, SessionBaseline, SessionBaselineInputs,
    SessionMcpAuthoringContext, SessionNetworkPolicy,
};
use awaken_session_contract::{
    IdempotencyRecord, PersistedSession, SessionExecutionState, SessionMutation,
    SessionMutationPayload,
};
use awaken_session_contract::{SessionRepository, SessionRepositoryReads};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
pub struct ExecutionFixture {
    pub authority: Arc<dyn CredentialExecutionAuthority>,
    sessions: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    queue: Arc<AnyCoordinatorDispatch>,
}
impl ExecutionFixture {
    pub async fn open(
        root: &std::path::Path,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let sessions = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(
                root.join("sessions.db").to_str().ok_or("path")?,
            )
            .await?,
        );
        let queue = Arc::new(AnyCoordinatorDispatch::from_durable_store(
            MemoryDispatchStore::new(clock),
            None,
        ));
        let authority = Arc::new(
            awaken_coordinator::credential_execution::SessionCredentialExecutionAuthority::new(
                sessions.clone(),
                queue.clone(),
            ),
        );
        let fixture = Self {
            authority,
            sessions,
            queue,
        };
        for suffix in ["a", "b"] {
            fixture.create(suffix).await?;
        }
        Ok(fixture)
    }
    async fn create(&self, suffix: &str) -> Result<(), Box<dyn std::error::Error>> {
        let baseline = SessionBaseline::compile(SessionBaselineInputs {
            environment: EnvironmentSnapshot {
                environment_id: "env_e2e".into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: EnvironmentFingerprint("fixture-env".into()),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: SessionNetworkPolicy::None,
                credential_realization:
                    awaken_credential_contract::CredentialRealizationProfile::self_hosted_acp(),
            },
            runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
            mcp_authoring: SessionMcpAuthoringContext::default(),
            agent_id: "agent_a".into(),
            agent_revision: None,
            model: "qualification-only".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        });
        let id = format!("session_{suffix}");
        let session = PersistedSession::frozen_with_budget(
            id.as_str(),
            baseline,
            Default::default(),
            Default::default(),
            None,
            BTreeMap::new(),
            Default::default(),
            Default::default(),
        );
        let receipt = SessionMutationPayload::default_create_idempotency(&session);
        self.sessions
            .create("workspace-e2e", session, receipt, Vec::new())
            .await?;
        let mut activation = awaken_dispatch_testkit::worker_http::activation(suffix);
        activation.run_id = awaken_agent_contract::agent::run::Id(format!("run_{suffix}"));
        activation.thread_id = awaken_agent_contract::agent::thread::Id(id);
        activation.snapshot.root_agent_id =
            awaken_runtime_contract::snapshot::AgentId("agent_a".into());
        let request = RunDispatch::new(activation)
            .with_execution_scope(awaken_agent_contract::ExecutionScopeRef(
                "workspace-e2e".into(),
            ))
            .with_placement(PlacementRequirements::remote_preferred());
        let run = request.activation.run_id.clone();
        self.queue.enqueue(request).await?;
        self.claim(&run, suffix).await?;
        Ok(())
    }
    async fn claim(
        &self,
        run: &awaken_agent_contract::agent::run::Id,
        suffix: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let worker = WorkerIdentity::new("credential-fixture", "boot", 1);
        let mut manifest = awaken_dispatch::WorkerManifest::default();
        manifest.runtime_protocol.min = 1;
        manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
        let snapshot = awaken_dispatch::WorkerSnapshot {
            identity: worker.clone(),
            state: awaken_dispatch::WorkerState::Ready,
            capability_fingerprint: manifest.fingerprint()?,
            manifest,
            in_flight: 0,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms: u64::MAX,
        };
        let claimed = self
            .queue
            .claim_run_compatible(run, &snapshot, 120_000)
            .await?
            .ok_or("no compatible claim")?;
        let claim = RunClaim::from(&claimed.lease);
        let binding = AttemptExecutionBinding::new(
            worker,
            "qualification",
            format!("execution_{suffix}_{}", claim.epoch),
        )?;
        self.queue.begin_attempt(&claim, Some(&binding)).await?;
        Ok(())
    }
    pub async fn context(
        &self,
        suffix: &str,
    ) -> Result<CredentialExecutionBinding, awaken_credential_contract::CredentialMaterialError>
    {
        self.authority
            .resolve_current(
                "workspace-e2e",
                &format!("session_{suffix}"),
                &format!("run_{suffix}"),
            )
            .await
    }
    pub async fn fault(&self, kind: &str) -> Result<(), Box<dyn std::error::Error>> {
        let b = self.context("a").await?;
        let claim = RunClaim {
            run_id: awaken_agent_contract::agent::run::Id(b.run_id),
            owner: b.claim_owner,
            epoch: b.claim_epoch.get(),
        };
        match kind {
            "cancel" => {
                self.queue.cancel(&claim.run_id).await?;
            }
            "takeover" => {
                self.queue.finish_attempt(&claim, None).await?;
                self.queue.relinquish_claim(&claim).await?;
                self.claim(&claim.run_id, "replacement").await?;
            }
            "terminate" => {
                let mut session = self
                    .sessions
                    .get(&awaken_session_types::SessionId::from("session_a"))
                    .await?;
                let revision = session.revision;
                session.execution = SessionExecutionState::Terminated;
                let payload = SessionMutationPayload::Replace(session);
                self.sessions
                    .commit(
                        "workspace-e2e",
                        SessionMutation {
                            expected_revision: revision,
                            idempotency: IdempotencyRecord {
                                key: "qualification-terminate".into(),
                                payload_hash: payload.stable_hash(),
                            },
                            payload,
                            lifecycle_facts: Vec::new(),
                        },
                    )
                    .await?;
            }
            _ => return Err("unsupported qualification mutation".into()),
        };
        Ok(())
    }
}
