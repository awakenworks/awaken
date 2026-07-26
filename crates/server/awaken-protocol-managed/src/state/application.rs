//! Session creation composition owned by the Managed application layer.
//!
//! Raw Session and published Agent MCP authoring enters here once. The Session
//! aggregate owns precedence/finalization; this module only preserves source
//! identity and commits the resulting frozen aggregate through the root CAS.

use super::*;
use crate::types::McpServer;
use awaken_session_contract::{
    ApplicationContributionError, ApplicationSessionContribution,
    ApplicationSessionContributionFailure, ApplicationSessionContributionPort,
    ApplicationSessionContributionReceipt, FrozenSessionProjection,
};

#[derive(Clone, Debug)]
pub(super) struct ManagedMcpCandidate {
    pub(super) server: McpServer,
    pub(super) published_credential: Option<(String, u64)>,
    pub(super) origin: awaken_session_contract::McpAttachmentOrigin,
}

/// Preserve every create-time authoring candidate and its actual source. The
/// Session aggregate, not array order or credential presence, resolves logical
/// name precedence and target conflicts after canonical normalization.
pub(super) fn initial_mcp_candidates(
    session: &[McpServer],
    agent: Option<&awaken_session_contract::AgentConfigView>,
) -> Vec<ManagedMcpCandidate> {
    let mut candidates =
        Vec::with_capacity(session.len() + agent.map_or(0, |view| view.mcp_servers.len()));
    candidates.extend(session.iter().cloned().map(|server| ManagedMcpCandidate {
        server,
        published_credential: None,
        origin: awaken_session_contract::McpAttachmentOrigin::Session,
    }));
    if let Some(agent) = agent {
        candidates.extend(agent.mcp_servers.iter().map(|server| {
            ManagedMcpCandidate {
                server: McpServer {
                    name: server.name.clone(),
                    url: server.url.clone(),
                },
                published_credential: server
                    .credential_source_id
                    .clone()
                    .zip(server.credential_revision),
                origin: awaken_session_contract::McpAttachmentOrigin::Agent,
            }
        }));
    }
    candidates
}

impl ManagedState {
    pub(super) fn frozen_session_projection(
        owner_scope: String,
        session: &PersistedSession,
    ) -> Result<FrozenSessionProjection, ApplicationSessionContributionFailure> {
        let baseline = session.frozen_baseline().cloned().ok_or_else(|| {
            ApplicationSessionContributionFailure::Unavailable(
                "Session creation intent was not consumed".into(),
            )
        })?;
        let resources = session
            .resources
            .pending
            .clone()
            .unwrap_or_else(|| session.resources.active.clone());
        Ok(FrozenSessionProjection {
            workspace_id: owner_scope,
            revision: session.revision,
            baseline,
            resources,
            mcp: session.mcp.attachments.clone(),
        })
    }

    fn map_contribution_error(
        error: ApplicationContributionError,
    ) -> ApplicationSessionContributionFailure {
        match error {
            ApplicationContributionError::NotRequired => {
                ApplicationSessionContributionFailure::NotRequired
            }
            ApplicationContributionError::Conflict => {
                ApplicationSessionContributionFailure::Conflict
            }
            ApplicationContributionError::EmptyFingerprint => {
                ApplicationSessionContributionFailure::Invalid(error.to_string())
            }
        }
    }

    fn application_mcp_candidates(
        input: &awaken_session_contract::ApplicationSessionInput,
    ) -> Result<Vec<ManagedMcpCandidate>, ApplicationSessionContributionFailure> {
        input
            .mcp_inputs
            .iter()
            .cloned()
            .map(|value| {
                serde_json::from_value(value)
                    .map(|server| ManagedMcpCandidate {
                        server,
                        published_credential: None,
                        origin: awaken_session_contract::McpAttachmentOrigin::Application,
                    })
                    .map_err(|error| {
                        ApplicationSessionContributionFailure::Invalid(format!(
                            "application MCP input is malformed: {error}"
                        ))
                    })
            })
            .collect()
    }

    /// Consume exactly one durable preparation intent. Baseline freezing,
    /// Resource generation 1 and MCP generation 1 cross the same root revision;
    /// no external Runtime effect is allowed before this method commits.
    pub(super) async fn commit_compiled_session_creation(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        compiled: awaken_session_contract::CompiledSessionCreation,
    ) -> Result<PersistedSession, StateError> {
        match &session.baseline {
            awaken_session_contract::SessionBaselineState::Preparing(_) => {}
            awaken_session_contract::SessionBaselineState::Frozen(_) => {
                return Err(StateError::Run(RunError::internal(
                    "Session creation intent was already consumed",
                )));
            }
        }
        let holder = compiled
            .baseline
            .environment
            .credential_realization
            .mcp_holder
            .clone();
        let mut resources = awaken_session_contract::SessionResourceState::default();
        resources
            .prepare(&session.session_id, compiled.initial_resources)
            .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
        let mcp = awaken_session_contract::SessionMcpAttachmentSet::from_initial(
            compiled.initial_mcp,
            Some(holder),
        )
        .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        session.baseline = awaken_session_contract::SessionBaselineState::Frozen(compiled.baseline);
        session.resources = resources;
        session.mcp = mcp;
        self.commit_session_snapshot(owner_scope, session, "finalize-creation", Vec::new())
            .await
    }
}

#[async_trait::async_trait]
impl ApplicationSessionContributionPort for ManagedState {
    async fn contribute_application(
        &self,
        contribution: ApplicationSessionContribution,
    ) -> Result<ApplicationSessionContributionReceipt, ApplicationSessionContributionFailure> {
        if contribution.session_id.trim().is_empty() {
            return Err(ApplicationSessionContributionFailure::Invalid(
                "Session id is empty".into(),
            ));
        }
        let owner_scope = self
            .resolve_owner(&contribution.session_id)
            .await
            .ok_or(ApplicationSessionContributionFailure::NotFound)?;

        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let mut session = self
                .sessions_repo
                .get(&contribution.session_id)
                .await
                .ok_or(ApplicationSessionContributionFailure::NotFound)?;
            let intent = match &mut session.baseline {
                awaken_session_contract::SessionBaselineState::Frozen(baseline) => {
                    let receipt = baseline
                        .application
                        .as_ref()
                        .ok_or(ApplicationSessionContributionFailure::NotRequired)?;
                    let outcome = receipt
                        .verify_replay(&contribution.application_fingerprint, &contribution.input)
                        .map_err(Self::map_contribution_error)?;
                    return Ok(ApplicationSessionContributionReceipt {
                        outcome,
                        projection: Self::frozen_session_projection(owner_scope.clone(), &session)?,
                    });
                }
                awaken_session_contract::SessionBaselineState::Preparing(intent) => intent,
            };

            let outcome = intent
                .application
                .accept(
                    contribution.application_fingerprint.clone(),
                    contribution.input.clone(),
                )
                .map_err(Self::map_contribution_error)?;
            let ordered_vault_ids = intent.control.mcp_authoring.ordered_vault_ids.clone();
            let candidates = Self::application_mcp_candidates(&contribution.input)?;
            let application_mcp = self
                .normalize_mcp_drafts(candidates, &ordered_vault_ids)
                .await
                .map_err(|error| {
                    ApplicationSessionContributionFailure::Invalid(error.to_string())
                })?;
            let compiled = intent.clone().finalize(application_mcp).map_err(|error| {
                ApplicationSessionContributionFailure::Invalid(error.to_string())
            })?;

            match self
                .commit_compiled_session_creation(&owner_scope, session, compiled)
                .await
            {
                Ok(session) => {
                    return Ok(ApplicationSessionContributionReceipt {
                        outcome,
                        projection: Self::frozen_session_projection(owner_scope.clone(), &session)?,
                    });
                }
                Err(StateError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => continue,
                Err(StateError::Conflict) => {
                    return Err(ApplicationSessionContributionFailure::Conflict);
                }
                Err(error) => {
                    return Err(ApplicationSessionContributionFailure::Unavailable(
                        error.to_string(),
                    ));
                }
            }
        }
        Err(ApplicationSessionContributionFailure::Conflict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_credential_contract::{
        CredentialRealizationProfile, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_session_contract::{
        ApplicationContributionOutcome, IdempotencyRecord, OutcomeReport, RunError, SessionInit,
        SessionRuntime, StepOutcome, ToolPermissionDecision,
    };

    struct NoopRuntime;

    #[async_trait::async_trait]
    impl SessionRuntime for NoopRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            Err(RunError::internal("unused"))
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            Err(RunError::internal("unused"))
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: &str,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            Err(RunError::internal("unused"))
        }

        async fn prepare_session(&self, _thread: &str, _init: SessionInit) -> Result<(), RunError> {
            Ok(())
        }

        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            Ok(())
        }

        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeReport, RunError> {
            Err(RunError::internal("unused"))
        }

        fn model(&self) -> String {
            "test-model".into()
        }
    }

    fn creation_intent(
        application: awaken_session_contract::ApplicationContributionState,
        initial_mcp: Vec<awaken_session_contract::McpAttachmentDraft>,
    ) -> awaken_session_contract::SessionCreationIntent {
        let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "test.worker");
        awaken_session_contract::SessionCreationIntent {
            control: awaken_session_contract::ControlSessionCreationInputs {
                environment: awaken_session_contract::EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_session_contract::env_registry::EnvironmentRevision(1),
                    config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                        "env-fingerprint".into(),
                    ),
                    sandbox: serde_json::json!({"isolation": "namespace"}),
                    network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    credential_realization: CredentialRealizationProfile {
                        inference_holder: holder.clone(),
                        mcp_holder: holder.clone(),
                        resource_holder: holder,
                    },
                },
                agent_id: "agent".into(),
                model: "model".into(),
                runtime: None,
                mcp_authoring: Default::default(),
                delegate_ids: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                resources: Default::default(),
                initial_mcp,
            },
            application,
        }
    }

    async fn state_with_intent(
        id: &str,
        intent: awaken_session_contract::SessionCreationIntent,
    ) -> ManagedState {
        let state = ManagedState::new(NoopRuntime);
        let session = PersistedSession {
            session_id: id.into(),
            revision: Default::default(),
            baseline: awaken_session_contract::SessionBaselineState::Preparing(intent),
            title: None,
            metadata: Default::default(),
            agent_tools: None,
            environment_binding: None,
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            status: "preparing".into(),
            archived_at: None,
        };
        let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
        state
            .sessions_repo
            .create(
                "workspace",
                session,
                IdempotencyRecord {
                    key: format!("test-create:{id}"),
                    payload_hash: payload.stable_hash(),
                },
                Vec::new(),
            )
            .await
            .unwrap();
        state
    }

    #[derive(Clone, Copy)]
    enum ContributionRule {
        Commit,
        Replay,
        SamePlanDifferentInput,
        DifferentPlan,
        NotRequired,
        EmptyFingerprint,
        MalformedMcp,
        ConflictingMcp,
        MissingSession,
    }

    #[tokio::test]
    async fn contribution_service_cases_are_generated_from_the_decision_table() {
        // Cause graph:
        // authenticated boundary (tested by the transport table) -> Session exists
        // -> Preparing requires input -> fingerprint is non-empty -> MCP input parses
        // and normalizes -> one root CAS freezes baseline/generation 1. A frozen
        // receipt admits only an exact replay; every rejected rule leaves the root
        // revision and preparation state unchanged.
        //
        // | Rule | Exists | State | Fingerprint | Input | MCP | Effect |
        // |---|---|---|---|---|---|---|
        // | S1 | T | Required | new | valid | valid | commit Frozen at rev 2 |
        // | S2 | T | Frozen | same | same | valid | replay, no new revision |
        // | S3 | T | Frozen | same | different | - | conflict, no write |
        // | S4 | T | Frozen | different | any | - | conflict, no write |
        // | S5 | T | Absent | non-empty | valid | - | not required, no write |
        // | S6 | T | Required | empty | valid | - | invalid, no write |
        // | S7 | T | Required | non-empty | valid | malformed | invalid, no write |
        // | S8 | T | Required | non-empty | valid | target conflict | invalid, no write |
        // | S9 | F | - | non-empty | valid | valid | not found |
        for rule in [
            ContributionRule::Commit,
            ContributionRule::Replay,
            ContributionRule::SamePlanDifferentInput,
            ContributionRule::DifferentPlan,
            ContributionRule::NotRequired,
            ContributionRule::EmptyFingerprint,
            ContributionRule::MalformedMcp,
            ContributionRule::ConflictingMcp,
            ContributionRule::MissingSession,
        ] {
            let id = format!("session-{}", rule as u8);
            let initial_mcp = matches!(rule, ContributionRule::ConflictingMcp)
                .then(|| awaken_session_contract::McpAttachmentDraft {
                    name: "control".into(),
                    target: awaken_session_contract::McpTarget::parse_http("https://same.example")
                        .unwrap(),
                    credential: None,
                    origin: awaken_session_contract::McpAttachmentOrigin::Session,
                })
                .into_iter()
                .collect();
            let application = if matches!(rule, ContributionRule::NotRequired) {
                awaken_session_contract::ApplicationContributionState::Absent
            } else {
                awaken_session_contract::ApplicationContributionState::Required
            };
            let state = state_with_intent(&id, creation_intent(application, initial_mcp)).await;
            let mut input = awaken_session_contract::ApplicationSessionInput {
                prompts: vec!["application prompt".into()],
                ..Default::default()
            };
            if matches!(rule, ContributionRule::MalformedMcp) {
                input.mcp_inputs.push(serde_json::json!({"name": 7}));
            } else if matches!(rule, ContributionRule::ConflictingMcp) {
                input.mcp_inputs.push(serde_json::json!({
                    "name": "application",
                    "url": "https://same.example"
                }));
            }
            let fingerprint = if matches!(rule, ContributionRule::EmptyFingerprint) {
                " "
            } else {
                "plan-a"
            };
            let contribution = ApplicationSessionContribution {
                session_id: if matches!(rule, ContributionRule::MissingSession) {
                    "missing".into()
                } else {
                    id.clone()
                },
                application_fingerprint: fingerprint.into(),
                input: input.clone(),
            };

            if matches!(
                rule,
                ContributionRule::Replay
                    | ContributionRule::SamePlanDifferentInput
                    | ContributionRule::DifferentPlan
            ) {
                state
                    .contribute_application(contribution.clone())
                    .await
                    .expect("table setup commits S1");
            }
            let mut attempted = contribution;
            if matches!(rule, ContributionRule::SamePlanDifferentInput) {
                attempted.input.prompts = vec!["different".into()];
            }
            if matches!(rule, ContributionRule::DifferentPlan) {
                attempted.application_fingerprint = "plan-b".into();
            }
            let result = state.contribute_application(attempted).await;

            match rule {
                ContributionRule::Commit => {
                    let receipt = result.expect("S1");
                    assert_eq!(receipt.outcome, ApplicationContributionOutcome::Committed);
                    assert_eq!(receipt.projection.revision.0, 2);
                    assert_eq!(
                        receipt.projection.baseline.prompts,
                        vec!["application prompt"]
                    );
                }
                ContributionRule::Replay => {
                    let receipt = result.expect("S2");
                    assert_eq!(receipt.outcome, ApplicationContributionOutcome::Replayed);
                    assert_eq!(receipt.projection.revision.0, 2);
                }
                ContributionRule::SamePlanDifferentInput | ContributionRule::DifferentPlan => {
                    assert_eq!(
                        result.unwrap_err(),
                        ApplicationSessionContributionFailure::Conflict
                    );
                }
                ContributionRule::NotRequired => {
                    assert_eq!(
                        result.unwrap_err(),
                        ApplicationSessionContributionFailure::NotRequired
                    );
                }
                ContributionRule::EmptyFingerprint
                | ContributionRule::MalformedMcp
                | ContributionRule::ConflictingMcp => {
                    assert!(
                        matches!(
                            result,
                            Err(ApplicationSessionContributionFailure::Invalid(_))
                        ),
                        "rule {} returned {result:?}",
                        rule as u8
                    );
                }
                ContributionRule::MissingSession => {
                    assert_eq!(
                        result.unwrap_err(),
                        ApplicationSessionContributionFailure::NotFound
                    );
                }
            }

            if !matches!(rule, ContributionRule::MissingSession) {
                let persisted = state.sessions_repo.get(&id).await.unwrap();
                let expected_revision = if matches!(
                    rule,
                    ContributionRule::Commit
                        | ContributionRule::Replay
                        | ContributionRule::SamePlanDifferentInput
                        | ContributionRule::DifferentPlan
                ) {
                    2
                } else {
                    1
                };
                assert_eq!(
                    persisted.revision.0, expected_revision,
                    "rule {}",
                    rule as u8
                );
                assert_eq!(
                    persisted.frozen_baseline().is_some(),
                    expected_revision == 2,
                    "rule {}",
                    rule as u8
                );
            }
        }
    }

    fn server(name: &str, url: &str) -> McpServer {
        McpServer {
            name: name.into(),
            url: url.into(),
        }
    }

    fn agent_server(
        name: &str,
        url: &str,
        credential: Option<(&str, u64)>,
    ) -> awaken_session_contract::AgentMcpServerView {
        awaken_session_contract::AgentMcpServerView {
            name: name.into(),
            url: url.into(),
            credential_source_id: credential.map(|(id, _)| id.to_string()),
            credential_revision: credential.map(|(_, revision)| revision),
        }
    }

    #[test]
    fn initial_mcp_source_cases_follow_the_decision_table() {
        // Cause graph:
        // Session authoring -> Session origin; Agent authoring -> Agent origin
        // independently of credential presence; collisions remain candidates so
        // the one aggregate precedence resolver decides after normalization.
        //
        // | Rule | Source | Credential | Name/target collision | Effect |
        // |---|---|---|---|---|
        // | C1 | Session | absent | none | Session candidate |
        // | C2 | Agent | present | none | Agent candidate + exact pin |
        // | C3 | Agent | absent | none | Agent candidate, never Session |
        // | C4 | Session+Agent | any | same name | retain both for precedence |
        // | C5 | Session+Agent | any | same target/different name | retain both for conflict check |
        let view = awaken_session_contract::AgentConfigView {
            model: None,
            system: None,
            tool_ids: Vec::new(),
            mcp_servers: vec![
                agent_server("secured", "https://secured.example", Some(("cred", 7))),
                agent_server("public", "https://public.example", None),
                agent_server("same-name", "https://agent.example", None),
                agent_server("agent-alias", "https://same.example", None),
            ],
            skill_ids: Vec::new(),
            delegate_ids: Vec::new(),
            resources: Vec::new(),
        };
        let candidates = initial_mcp_candidates(
            &[
                server("inline", "https://inline.example"),
                server("same-name", "https://session.example"),
                server("session-alias", "https://same.example"),
            ],
            Some(&view),
        );
        assert_eq!(candidates.len(), 7);
        assert_eq!(
            candidates[0].origin,
            awaken_session_contract::McpAttachmentOrigin::Session,
            "C1"
        );
        assert_eq!(
            candidates[3].published_credential,
            Some(("cred".into(), 7)),
            "C2"
        );
        assert_eq!(
            candidates[4].origin,
            awaken_session_contract::McpAttachmentOrigin::Agent,
            "C3"
        );
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| candidate.server.name == "same-name")
                .count(),
            2,
            "C4"
        );
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| candidate.server.url == "https://same.example")
                .count(),
            2,
            "C5"
        );
    }
}
