//! Managed wire mapping for Session creation composition.
//!
//! Raw Session and published Agent MCP DTOs enter here once. This module only
//! preserves source identity and translates them into protocol-neutral MCP
//! candidates. Session finalization and root CAS belong exclusively to
//! [`awaken_session_application::SessionApplication`].

#[cfg(test)]
use super::*;
use crate::types::McpServer;
use crate::types::agent::AgentMcpServer;
use awaken_session_application::{McpAttachmentCandidate, McpAttachmentCandidateTarget};
#[cfg(test)]
use awaken_session_contract::{
    ApplicationSessionContribution, ApplicationSessionContributionApi,
    ApplicationSessionContributionFailure,
};

pub(super) fn agent_mcp_candidate(
    server: AgentMcpServer,
    origin: awaken_session_contract::McpAttachmentOrigin,
) -> McpAttachmentCandidate {
    match server {
        AgentMcpServer::Url {
            name,
            url,
            prompts_as_skills,
        } => McpAttachmentCandidate {
            name,
            target: McpAttachmentCandidateTarget::HttpUrl(url),
            prompts_as_skills,
            published_credential: None,
            origin,
        },
        AgentMcpServer::SandboxStdio {
            name,
            command,
            args,
            prompts_as_skills,
        } => McpAttachmentCandidate {
            name,
            target: McpAttachmentCandidateTarget::SandboxStdio { command, args },
            prompts_as_skills,
            published_credential: None,
            origin,
        },
    }
}

/// Preserve every create-time authoring candidate and its actual source. The
/// Session aggregate, not array order or credential presence, resolves logical
/// name precedence and target conflicts after canonical normalization.
pub(super) fn initial_mcp_candidates(
    session: &[McpServer],
    agent: Option<&awaken_executable_agent_contract::ExecutableAgentSessionProfile>,
    agent_override: Option<&[AgentMcpServer]>,
) -> Vec<McpAttachmentCandidate> {
    let agent_len = agent_override.map_or_else(
        || agent.map_or(0, |view| view.mcp_servers.len()),
        <[AgentMcpServer]>::len,
    );
    let mut candidates = Vec::with_capacity(session.len() + agent_len);
    candidates.extend(
        session
            .iter()
            .cloned()
            .map(|server| McpAttachmentCandidate {
                name: server.name,
                target: McpAttachmentCandidateTarget::HttpUrl(server.url),
                prompts_as_skills: server.prompts_as_skills,
                published_credential: None,
                origin: awaken_session_contract::McpAttachmentOrigin::Session,
            }),
    );
    if let Some(agent_override) = agent_override {
        candidates.extend(agent_override.iter().cloned().map(|server| {
            agent_mcp_candidate(server, awaken_session_contract::McpAttachmentOrigin::Agent)
        }));
    } else if let Some(agent) = agent {
        candidates.extend(agent.mcp_servers.iter().map(|server| {
            McpAttachmentCandidate {
                name: server.name.clone(),
                target: McpAttachmentCandidateTarget::Normalized(server.target.clone()),
                prompts_as_skills: server.prompts_as_skills,
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
            _content: Vec<ContentBlock>,
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
                    revision: awaken_environment_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                        "env-fingerprint".into(),
                    ),
                    sandbox: serde_json::json!({"isolation": "namespace"}),
                    sandbox_provisioning: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    credential_realization: CredentialRealizationProfile {
                        inference_holder: holder.clone(),
                        mcp_holder: holder.clone(),
                        resource_holder: holder,
                    },
                },
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                agent_id: "agent".into(),
                model: "model".into(),
                execution_model_ref: "model".into(),
                runtime: None,
                mcp_authoring: Default::default(),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
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
            tools: Default::default(),
            activity_epoch: 0,
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            status: "preparing".into(),
            archived_at: None,
        };
        let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
        state
            .application
            .session_repository()
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
        // revision and preparation state unchanged. The projection carries both
        // the root revision and the independently-owned Resource generation.
        //
        // | Rule | Exists | State | Fingerprint | Input | MCP | Effect |
        // |---|---|---|---|---|---|---|
        // | S1 | T | Required | new | valid | valid | root rev 2 + Resource gen 1 |
        // | S2 | T | Frozen | same | same | valid | replay same root/resource revisions |
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
                    prompts_as_skills: false,
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
                    .application
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
            let result = state.application.contribute_application(attempted).await;

            match rule {
                ContributionRule::Commit => {
                    let receipt = result.expect("S1");
                    assert_eq!(receipt.outcome, ApplicationContributionOutcome::Committed);
                    assert_eq!(receipt.projection.revision.0, 2);
                    assert_eq!(receipt.projection.resource_revision, 1);
                    assert_eq!(
                        receipt.projection.baseline.prompts,
                        vec!["application prompt"]
                    );
                }
                ContributionRule::Replay => {
                    let receipt = result.expect("S2");
                    assert_eq!(receipt.outcome, ApplicationContributionOutcome::Replayed);
                    assert_eq!(receipt.projection.revision.0, 2);
                    assert_eq!(receipt.projection.resource_revision, 1);
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
                let persisted = state
                    .application
                    .session_repository()
                    .get(&id)
                    .await
                    .unwrap();
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

    #[tokio::test]
    async fn contribution_preserves_a_file_attached_while_session_is_preparing() {
        let state = ManagedState::new(NoopRuntime);
        let session = state
            .create_session(
                crate::types::SessionCreateParams {
                    agent: crate::types::AgentRef::Id("assistant".into()),
                    initial_events: Vec::new(),
                    application_contribution_required: true,
                    environment_id: None,
                    title: None,
                    metadata: Default::default(),
                    mcp_servers: Vec::new(),
                    vault_ids: Vec::new(),
                    resources: Vec::new(),
                },
                Some("workspace".into()),
            )
            .await
            .expect("create preparing Session");
        let id = session.id;
        state
            .create_resource(
                &id,
                serde_json::from_value(serde_json::json!({
                    "type": "file",
                    "file_id": "file-product-design",
                    "mount_path": "/mnt/fab/product.pdf"
                }))
                .unwrap(),
            )
            .await
            .expect("attach before first Run");

        let receipt = state
            .application
            .contribute_application(ApplicationSessionContribution {
                session_id: id,
                application_fingerprint: "flow-plan".into(),
                input: Default::default(),
            })
            .await
            .expect("freeze Session without dropping upload");

        assert_eq!(receipt.projection.resources.inputs.len(), 1);
        assert_eq!(
            receipt.projection.resources.inputs[0].mount_path,
            "/mnt/fab/product.pdf"
        );
        assert!(matches!(
            receipt.projection.resources.inputs[0].source,
            awaken_session_contract::ResolvedInputSource::File { ref file_id }
                if file_id.as_str() == "file-product-design"
        ));
    }

    fn server(name: &str, url: &str) -> McpServer {
        McpServer {
            name: name.into(),
            url: url.into(),
            prompts_as_skills: false,
        }
    }

    fn agent_server(
        name: &str,
        url: &str,
        credential: Option<(&str, u64)>,
    ) -> awaken_executable_agent_contract::ExecutableAgentMcpServer {
        awaken_executable_agent_contract::ExecutableAgentMcpServer {
            name: name.into(),
            target: awaken_session_contract::McpTarget::parse_http(url).unwrap(),
            prompts_as_skills: false,
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
        let view = awaken_executable_agent_contract::ExecutableAgentSessionProfile {
            environment: None,
            model: None,
            execution_model_ref: None,
            backend_ref: "genai".into(),
            system: None,
            tool_ids: Vec::new(),
            toolsets: Vec::new(),
            client_tools: Vec::new(),
            mcp_servers: vec![
                agent_server("secured", "https://secured.example", Some(("cred", 7))),
                agent_server("public", "https://public.example", None),
                agent_server("same-name", "https://agent.example", None),
                agent_server("agent-alias", "https://same.example", None),
            ],
            skills: Vec::new(),
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
            None,
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
                .filter(|candidate| candidate.name == "same-name")
                .count(),
            2,
            "C4"
        );
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| {
                    match &candidate.target {
                        McpAttachmentCandidateTarget::HttpUrl(url) => url == "https://same.example",
                        McpAttachmentCandidateTarget::SandboxStdio { .. } => false,
                        McpAttachmentCandidateTarget::Normalized(target) => {
                            target.http_url() == Some("https://same.example")
                        }
                    }
                })
                .count(),
            2,
            "C5"
        );

        // Agent overrides replace, rather than overlay, the published Agent MCP
        // set. Session declarations remain higher-precedence candidates and the
        // same aggregate resolver handles any collision.
        //
        // | Rule | Override             | Session input | Effect |
        // |---|---|---|---|
        // | C6 | one replacement server | present       | Session + replacement Agent candidate |
        // | C7 | empty                  | present       | Session candidate only |
        let replacement = [AgentMcpServer::Url {
            name: "replacement".into(),
            url: "https://replacement.example".into(),
            prompts_as_skills: false,
        }];
        let replaced = initial_mcp_candidates(
            &[server("inline", "https://inline.example")],
            Some(&view),
            Some(&replacement),
        );
        assert_eq!(replaced.len(), 2, "C6");
        assert_eq!(replaced[1].name, "replacement", "C6");
        assert_eq!(
            replaced[1].origin,
            awaken_session_contract::McpAttachmentOrigin::Agent,
            "C6"
        );
        assert!(
            replaced.iter().all(|candidate| candidate.name != "secured"),
            "C6"
        );
        let cleared = initial_mcp_candidates(
            &[server("inline", "https://inline.example")],
            Some(&view),
            Some(&[]),
        );
        assert_eq!(cleared.len(), 1, "C7");
        assert_eq!(cleared[0].name, "inline", "C7");
    }
}
