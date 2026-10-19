//! Actual Session acceptance and recovery, using the existing revisioned source.
use super::*;
use awaken_executable_agent_contract::{
    ExecutableAgentProfileSource, ExecutableAgentSessionProfile,
};
use awaken_session_repository_contract::LifecycleOutbox as _;
use awaken_session_state_contract::{
    SessionScanQuery, SessionUpdateAgentConfiguration, SessionUpdateHistory,
};
use awaken_session_types::SessionRevision;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// An observation/fault decorator around the original fixture, not another
/// profile factory. Each profile/snapshot still comes from RevisionedProfiles.
struct ObservedProfiles {
    original: RevisionedProfiles,
    reads: std::sync::Mutex<Vec<(String, Option<u64>)>>,
    withdrawn: AtomicBool,
    missing: std::sync::Mutex<std::collections::BTreeSet<(String, u64)>>,
    unpinned_delegate: bool,
    advance_after_child_read: AtomicBool,
}

impl ObservedProfiles {
    fn new(unpinned_delegate: bool) -> Self {
        Self {
            original: RevisionedProfiles(AtomicU64::new(7)),
            reads: Default::default(),
            withdrawn: AtomicBool::new(false),
            missing: Default::default(),
            unpinned_delegate,
            advance_after_child_read: AtomicBool::new(false),
        }
    }

    fn observe(
        &self,
        agent_id: &str,
        revision: Option<u64>,
        profile: Option<ExecutableAgentSessionProfile>,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.reads
            .lock()
            .unwrap()
            .push((agent_id.to_owned(), revision));
        if self.withdrawn.load(Ordering::SeqCst)
            || revision.is_some_and(|version| {
                self.missing
                    .lock()
                    .unwrap()
                    .contains(&(agent_id.to_owned(), version))
            })
        {
            return None;
        }
        let mut profile = profile?;
        if self.unpinned_delegate && agent_id == "coordinator" {
            // Deliberate original input partition: a fresh declared delegate
            // has no exact revision. Its current value must be sampled once.
            profile.delegates[0].source_revision = None;
        }
        if revision.is_none()
            && agent_id == "researcher"
            && self.advance_after_child_read.swap(false, Ordering::SeqCst)
        {
            self.original
                .0
                .store(profile.source_revision + 1, Ordering::SeqCst);
        }
        Some(profile)
    }

    fn reads(&self) -> Vec<(String, Option<u64>)> {
        self.reads.lock().unwrap().clone()
    }
}

impl ExecutableAgentProfileSource for ObservedProfiles {
    fn session_profile_in(
        &self,
        workspace: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.observe(
            agent_id,
            None,
            self.original.session_profile_in(workspace, agent_id),
        )
    }

    fn session_profile_at_revision_in(
        &self,
        workspace: &str,
        agent_id: &str,
        revision: u64,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.observe(
            agent_id,
            Some(revision),
            self.original
                .session_profile_at_revision_in(workspace, agent_id, revision),
        )
    }

    fn executable_snapshot_at_revision_in(
        &self,
        workspace: &str,
        agent_id: &str,
        revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        self.original
            .executable_snapshot_at_revision_in(workspace, agent_id, revision)
    }

    fn agent_unavailable_in(&self, _workspace: &str, agent_id: &str) -> bool {
        self.withdrawn.load(Ordering::SeqCst) && agent_id == "coordinator"
    }
}

fn request(agent: serde_json::Value) -> SessionCreateParams {
    serde_json::from_value(serde_json::json!({
        "agent": agent, "environment_id": "env_local"
    }))
    .unwrap()
}

async fn roots(
    repo: &impl awaken_session_repository_contract::SessionStore,
) -> Vec<PersistedSession> {
    repo.scan(SessionScanQuery::Workspace {
        workspace_id: DEFAULT_SCOPE,
    })
    .await
    .unwrap()
    .into_sessions()
}

#[tokio::test]
async fn accepted_a_and_b_snapshots_survive_catalog_withdrawal_and_cold_peer() {
    // WU6-A/B cause graph: C1 true Managed create accepts head7; C2 head8
    // accepts explicit equal-model controls, system Clear, custom tools/MCP and
    // explicit empty Skills and the required one-to-one MCP/toolset pairing;
    // C3 all display catalog reads later disappear;
    // C4 a fresh Managed instance reads the SAME durable Session/Runtime stores.
    // A/B+C3+C4 -> each complete wire root stays equal to its own accepted value,
    // both rosters keep their original exact child revision, and no current/exact
    // profile query runs during recovery. New creation after withdrawal refuses.
    // This existing Runtime's Skill resolver is empty: these cases cover the
    // accepted empty/clear selection, never claim a physical nonempty Skill pin.
    let repo = Arc::new(ephemeral_session_repo().await);
    let profiles = Arc::new(ObservedProfiles::new(false));
    let runtime = RehydrateFake::default();
    let state = ManagedState::new_with_mcp(runtime.clone())
        .await
        .with_config_source(profiles.clone())
        .with_session_repo(repo.clone());
    let a = state
        .create_session(request(serde_json::json!("coordinator")), DEFAULT_SCOPE)
        .await
        .unwrap();
    profiles.original.0.store(8, Ordering::SeqCst);
    let b = state.create_session(request(serde_json::json!({
        "type": "agent_with_overrides", "id": "coordinator", "version": 8,
        "model": {"id": "test-model", "effort": "high"}, "system": null,
        "skills": [],
        "tools": [{"type": "custom", "name": "accepted-b", "description": "B only",
            "input_schema": {"type": "object"}},
            {"type": "mcp_toolset", "mcp_server_name": "accepted-b"}],
        "mcp_servers": [{"type": "url", "name": "accepted-b", "url": "https://accepted-b.example.test/mcp"}]
    })), DEFAULT_SCOPE).await.unwrap();
    assert_eq!(a.agent.name, "coordinator-v7");
    assert_eq!(a.agent.version, 7);
    assert_eq!(a.agent.model.speed, Some(crate::types::ModelSpeed::Fast));
    assert_eq!(a.agent.system.as_deref(), Some("coordinator-v7 system"));
    assert_eq!(b.agent.name, "coordinator-v8");
    assert_eq!(
        b.agent.model.speed, None,
        "controls replace rather than merge"
    );
    // WU6-B2: Session overrides accept effort on input but the existing wire
    // normalizer selects default effort. History records that EFFECTIVE value;
    // the absent speed also replaces the inherited Fast instead of merging.
    assert_eq!(
        b.agent.model.effort, None,
        "canonical Session effort normalization"
    );
    assert_eq!(b.agent.system, None, "accepted Clear survives as null");
    assert!(b.agent.skills.is_empty());
    // WU6-B3 the neutral owner stores toolsets and clients separately; the
    // existing wire projector emits toolsets first. Freeze its accepted order,
    // including both original names, rather than the request union interleave.
    assert!(
        matches!(&b.agent.tools[..], [crate::tools::AgentTool::McpToolset { mcp_server_name, .. }, crate::tools::AgentTool::Custom {name, ..}] if name == "accepted-b" && mcp_server_name == "accepted-b"),
        "canonical toolsets-before-custom order: {:?}",
        b.agent.tools
    );
    assert_eq!(b.agent.mcp_servers[0].name, "accepted-b");
    for (session, version) in [(&a, 7), (&b, 8)] {
        let child = session.agent.multiagent.as_ref().unwrap().agents[0]
            .as_agent()
            .unwrap();
        assert_eq!(child.name, format!("researcher-v{version}"));
        assert_eq!(child.version, version);
    }
    // Run the original committed reducer once before comparing full views:
    // it legitimately turns unknown usage into its observed zero counters.
    // Agent configuration remains byte-for-byte the accepted create response.
    let mut warmed = Vec::new();
    for accepted in [&a, &b] {
        state.ensure_session(&accepted.id).await.unwrap();
        let projected = state.get_session(&accepted.id).unwrap();
        assert_eq!(
            serde_json::to_value(&projected.agent).unwrap(),
            serde_json::to_value(&accepted.agent).unwrap()
        );
        warmed.push(projected);
    }
    let durable = roots(repo.as_ref()).await;
    assert_eq!(durable.len(), 2);
    for root in &durable {
        let configuration = SessionUpdateHistory::agent_configuration(root).unwrap();
        if root.session_id.as_str() == b.id {
            assert_eq!(
                configuration.model.source,
                awaken_session_configuration_contract::SessionModelSource::Override
            );
            assert_eq!(
                configuration.model.inference.effort, None,
                "WU6-B2 effective stored effort"
            );
            assert_eq!(
                configuration.model.inference.speed, None,
                "WU6-B2 complete replacement"
            );
        }
        assert!(
            SessionUpdateHistory::for_session(root)
                .unwrap()
                .facts()
                .is_empty()
        );
    }
    profiles.withdrawn.store(true, Ordering::SeqCst);
    assert!(
        state
            .create_session(request(serde_json::json!("coordinator")), DEFAULT_SCOPE)
            .await
            .is_err()
    );
    let reads = profiles.reads();
    let cold = ManagedState::new_with_mcp(runtime)
        .await
        .with_config_source(profiles.clone())
        .with_session_repo(repo.clone());
    for accepted in warmed {
        state.ensure_session(&accepted.id).await.unwrap();
        cold.ensure_session(&accepted.id).await.unwrap();
        let expected = serde_json::to_value(&accepted).unwrap();
        assert_eq!(
            serde_json::to_value(state.get_session(&accepted.id).unwrap()).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::to_value(cold.get_session(&accepted.id).unwrap()).unwrap(),
            expected
        );
    }
    assert_eq!(
        profiles.reads(),
        reads,
        "recorded snapshots do not reread unavailable catalog"
    );
    assert_eq!(
        roots(repo.as_ref()).await,
        durable,
        "recovery writes no new accepted facts"
    );
}

#[tokio::test]
async fn explicit_missing_root_or_delegate_publication_never_follows_current() {
    // WU6-E1/E2: exact root or child rev7 is unavailable while current head8
    // exists. Both real create requests refuse before root/outbox acceptance;
    // no fallback current query may make the missing exact reference succeed.
    // The original immutable snapshot adapter remains available, so this cause
    // tests profile pin selection rather than an unrelated execution failure.
    // The shared SessionQueries selector owns this exact/current decision;
    // these real create rows expose a fallback through its observed read log.
    for missing_agent in ["coordinator", "researcher"] {
        let repo = Arc::new(ephemeral_session_repo().await);
        let profiles = Arc::new(ObservedProfiles::new(false));
        profiles.original.0.store(8, Ordering::SeqCst);
        profiles
            .missing
            .lock()
            .unwrap()
            .insert((missing_agent.into(), 7));
        let state = ManagedState::new_with_mcp(RehydrateFake::default())
            .await
            .with_config_source(profiles.clone())
            .with_session_repo(repo.clone());
        let error = state
            .create_session(
                request(serde_json::json!({
                    "type": "agent", "id": "coordinator", "version": 7
                })),
                DEFAULT_SCOPE,
            )
            .await
            .expect_err("missing exact publication cannot admit current head8");
        assert!(
            error.to_string().contains("unavailable"),
            "{missing_agent}: {error}"
        );
        assert!(roots(repo.as_ref()).await.is_empty());
        assert!(repo.pending_lifecycle().await.unwrap().is_empty());
        assert!(state.list_sessions().is_empty());
        assert!(
            profiles
                .reads()
                .iter()
                .all(|(_, revision)| revision.is_some()),
            "no current fallback"
        );
    }
}

#[tokio::test]
async fn fresh_unpinned_delegate_is_sampled_once_before_head_changes() {
    // WU6-N1: the original fresh root declares a delegate with revision None;
    // its first real current read returns7 and atomically changes the fixture
    // head to8. Accepted roster and Skill-total validation must consume that
    // SAME captured7, rather than another catalog read. Expected child reads=1
    // and root/child version7. This locks the real create path, not the capture
    // helper alone; the previous duplicate Skill-total query yields reads=2.
    // The shared selector must retain the original None=current lookup and
    // the later roster compiler must reuse that first captured value.
    let repo = Arc::new(ephemeral_session_repo().await);
    let profiles = Arc::new(ObservedProfiles::new(true));
    profiles
        .advance_after_child_read
        .store(true, Ordering::SeqCst);
    let state = ManagedState::new_with_mcp(RehydrateFake::default())
        .await
        .with_config_source(profiles.clone())
        .with_session_repo(repo.clone());
    let accepted = state
        .create_session(request(serde_json::json!("coordinator")), DEFAULT_SCOPE)
        .await
        .unwrap();
    assert_eq!(accepted.agent.version, 7);
    let child = accepted.agent.multiagent.as_ref().unwrap().agents[0]
        .as_agent()
        .unwrap();
    assert_eq!(child.version, 7);
    assert_eq!(child.name, "researcher-v7");
    assert_eq!(profiles.original.0.load(Ordering::SeqCst), 8);
    let reads = profiles.reads();
    assert_eq!(
        reads
            .iter()
            .filter(|(id, revision)| id == "researcher" && revision.is_none())
            .count(),
        1,
        "fresh accepted delegate must not be selected again for Skill-total validation"
    );
    let root = repo.get(&SessionId::from(&accepted.id)).await.unwrap();
    assert_eq!(
        SessionUpdateHistory::agent_configuration(&root)
            .unwrap()
            .multiagent
            .unwrap()
            .members
            .len(),
        1
    );
}

#[tokio::test]
async fn historic_missing_accepted_agent_keeps_tools_update_recovery_pending() {
    // WU6-L1: a genuine old serialized root has no accepted configuration or
    // update history; a later current catalog exists but is not old acceptance
    // evidence. A real tools update commits exactly one Unrecorded Agent fact,
    // then the original complete candidate publisher refuses RecoveryRequired.
    // Old visible cache/events stutter, root retains the committed update and
    // receipt, cold peer also refuses, and no initial configuration is backfilled.
    let repo = Arc::new(ephemeral_session_repo().await);
    let mut wire = serde_json::to_value(sample_persisted("historic-missing-accepted")).unwrap();
    for field in ["initial_agent_configuration", "update_history"] {
        wire.as_object_mut().unwrap().remove(field);
    }
    let legacy: PersistedSession = serde_json::from_value(wire).unwrap();
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, legacy).await;
    let before = repo
        .get(&SessionId::from("historic-missing-accepted"))
        .await
        .unwrap();
    let profiles = Arc::new(ObservedProfiles::new(false));
    profiles.original.0.store(8, Ordering::SeqCst);
    let runtime = RehydrateFake::default();
    let state = ManagedState::new_with_mcp(runtime.clone())
        .await
        .with_config_source(profiles.clone())
        .with_session_repo(repo.clone());
    state
        .ensure_session(before.session_id.as_str())
        .await
        .unwrap();
    let visible =
        serde_json::to_value(state.get_session(before.session_id.as_str()).unwrap()).unwrap();
    let events = serde_json::to_value(
        state
            .list_events(before.session_id.as_str(), None, None, false)
            .unwrap(),
    )
    .unwrap();
    let key = "historic-tools-update";
    let command = awaken_session_application::SessionUpdateCommand {
        title: None,
        metadata: None,
        budget: None,
        mcp_update: None,
        tools: Some(
            awaken_session_configuration_contract::SessionToolConfiguration {
                toolsets: Vec::new(),
                client_tools: vec![awaken_agent_contract::ClientToolDescriptor {
                    name: "accepted-new-tool".into(),
                    description: "New authoring only".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                }],
            },
        ),
        idempotency_key: Some(key.into()),
        request_fingerprint: "historic-tools-command".into(),
        if_match: Some(before.revision),
    };
    let error = state
        .update_session(before.session_id.as_str(), command)
        .await
        .unwrap_err();
    assert!(error.is_projection_recovery_required(), "{error}");
    assert_eq!(
        serde_json::to_value(state.get_session(before.session_id.as_str()).unwrap()).unwrap(),
        visible
    );
    assert_eq!(
        serde_json::to_value(
            state
                .list_events(before.session_id.as_str(), None, None, false)
                .unwrap()
        )
        .unwrap(),
        events
    );
    let committed = repo.get(&before.session_id).await.unwrap();
    assert_eq!(committed.revision, SessionRevision(before.revision.0 + 1));
    assert!(SessionUpdateHistory::agent_configuration(&committed).is_none());
    let history = SessionUpdateHistory::for_session(&committed).unwrap();
    assert_eq!(history.known_after_revision(), before.revision);
    assert_eq!(history.facts().len(), 1);
    assert_eq!(
        history.facts()[0].agent(),
        Some(&SessionUpdateAgentConfiguration::Unrecorded)
    );
    let receipt_key = format!(
        "managed:update-command:{}:{}",
        before.session_id,
        awaken_session_application::SessionApplication::update_operation_id(
            before.session_id.as_str(),
            key
        )
    );
    let receipt = repo
        .idempotency_receipt(&before.session_id, &receipt_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.committed_revision, committed.revision);
    assert_eq!(receipt.payload_hash, "historic-tools-command");
    let cold = ManagedState::new_with_mcp(runtime)
        .await
        .with_config_source(profiles)
        .with_session_repo(repo.clone());
    assert!(
        cold.ensure_session(before.session_id.as_str())
            .await
            .unwrap_err()
            .is_projection_recovery_required()
    );
    assert_eq!(repo.get(&before.session_id).await.unwrap(), committed);
}
