//! Managed capability capture keeps its original position among creation awaits.
use super::*;
use awaken_protocol_managed_testkit::RehydrateFake;
use awaken_session_contract::{
    ManagedListPriceError, ManagedListPriceProvider, ManagedListPriceRequest,
    ManagedListPriceSnapshot,
};
use awaken_session_contract::{SessionScanQuery, SessionUpdateHistory};
use awaken_session_execution_contract::{
    EnvironmentRuntime, SessionExecutionRoles, SessionExecutor,
};
use awaken_session_types::{RunError, RunErrorKind};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(30);

/// Observe and fault the original profile fixture, without another profile or
/// executable-snapshot factory. Exact snapshots retain the original revision.
struct ObservedBudgetProfiles {
    original: BudgetBackendRoster,
    current_revision: AtomicU64,
    exact_response_revision: u64,
    reads: Mutex<Vec<(String, Option<u64>)>>,
}

impl ObservedBudgetProfiles {
    fn new(exact_response_revision: u64) -> Self {
        Self {
            original: BudgetBackendRoster {
                root_backend: "genai",
                root_fallback_backend: None,
                exact_child_backend: Some("genai"),
                exact_child_fallback_backend: None,
                current_child_backend: Some("genai"),
                advisor_backend: None,
            },
            current_revision: AtomicU64::new(4),
            exact_response_revision,
            reads: Default::default(),
        }
    }

    fn selected(
        &self,
        workspace: &str,
        id: &str,
        revision: Option<u64>,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.reads.lock().unwrap().push((id.to_owned(), revision));
        let mut profile = match revision {
            Some(revision) => self
                .original
                .session_profile_at_revision_in(workspace, id, revision),
            None => self.original.session_profile_in(workspace, id),
        }?;
        if id == "budget-child" {
            profile.source_revision = if revision.is_some() {
                self.exact_response_revision
            } else {
                self.current_revision.load(Ordering::SeqCst)
            };
            profile.name = Some(format!("budget-child-v{}", profile.source_revision));
        }
        Some(profile)
    }

    fn child_reads(&self) -> Vec<(String, Option<u64>)> {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| id == "budget-child" || id == "missing")
            .cloned()
            .collect()
    }
}

impl ExecutableAgentProfileSource for ObservedBudgetProfiles {
    fn session_profile_in(
        &self,
        workspace: &str,
        id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.selected(workspace, id, None)
    }

    fn session_profile_at_revision_in(
        &self,
        workspace: &str,
        id: &str,
        revision: u64,
    ) -> Option<ExecutableAgentSessionProfile> {
        // Deliberately return the faulty source value. SessionQueries selects
        // this exact port; it does not validate the returned revision itself.
        self.selected(workspace, id, Some(revision))
    }

    fn executable_snapshot_at_revision_in(
        &self,
        workspace: &str,
        id: &str,
        revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        self.original
            .executable_snapshot_at_revision_in(workspace, id, revision)
    }
}

/// Gate the existing price fixture, retaining its complete snapshot factory.
struct GatedPrices {
    original: RecordingListPriceProvider,
    entered: Notify,
    release: Notify,
    requests: Mutex<Vec<Vec<String>>>,
    refusal: Option<ManagedListPriceError>,
}

impl GatedPrices {
    fn new(refusal: Option<ManagedListPriceError>) -> Self {
        Self {
            original: Default::default(),
            entered: Notify::new(),
            release: Notify::new(),
            requests: Default::default(),
            refusal,
        }
    }

    async fn wait_for_entry(&self) {
        tokio::time::timeout(WAIT, self.entered.notified())
            .await
            .expect("creation must reach the original later price await");
    }
}

#[async_trait::async_trait]
impl ManagedListPriceProvider for GatedPrices {
    async fn resolve_snapshot(
        &self,
        request: ManagedListPriceRequest,
    ) -> Result<ManagedListPriceSnapshot, ManagedListPriceError> {
        self.requests
            .lock()
            .unwrap()
            .push(request.model_refs.clone());
        self.entered.notify_one();
        tokio::time::timeout(WAIT, self.release.notified())
            .await
            .map_err(|_| {
                ManagedListPriceError::Unavailable("test price gate was not released".into())
            })?;
        if let Some(error) = &self.refusal {
            return Err(error.clone());
        }
        self.original.resolve_snapshot(request).await
    }
}

/// Override only the actual Skill port, preserving the co-located placement
/// and delegating all other Environment operations to their original owner.
struct RefusingSkills {
    original: Arc<dyn EnvironmentRuntime>,
}

#[async_trait::async_trait]
impl EnvironmentRuntime for RefusingSkills {
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_configuration_contract::SessionToolConfiguration,
    ) -> Result<(), RunError> {
        self.original.replace_session_tools(thread, tools).await
    }

    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError> {
        self.original
            .adopt_session_environment(agent, thread, binding)
            .await
    }

    async fn quiesce_session_environment(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_contract::SandboxGeneration,
        mcp: &[awaken_session_configuration_contract::mcp::McpGenerationRef],
    ) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
        self.original
            .quiesce_session_environment(
                thread,
                operation,
                source_effect_id,
                source_binding,
                generation,
                mcp,
            )
            .await
    }

    async fn resolve_session_skills(
        &self,
        _workspace: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<awaken_session_configuration_contract::resource::ResolvedSkillBinding>, RunError>
    {
        assert_eq!(skills.len(), 1, "R2 reaches the selected Skill port");
        Err(RunError::classified(
            "capture_test_skill_refused",
            "original Skill port refused selection",
        ))
    }
}

fn selected_roles(runtime: RehydrateFake) -> SessionExecutionRoles {
    let runtime = Arc::new(runtime);
    let selected = SessionExecutor::colocated(runtime.clone(), runtime);
    (*selected).clone()
}

fn state(
    roles: SessionExecutionRoles,
    repo: Arc<SqliteManagedSessionRepository>,
    profiles: Arc<ObservedBudgetProfiles>,
    prices: Arc<GatedPrices>,
) -> ManagedState {
    let clock = awaken_time::system_clock();
    let application = awaken_protocol_managed_testkit::session_application_from_parts(
        roles,
        repo,
        awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
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
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    );
    ManagedState::from_application(application, Default::default())
        .with_config_source(profiles)
        .with_managed_list_price_provider(prices)
}

fn budget_request(agent: Value) -> SessionCreateParams {
    serde_json::from_value(json!({
        "agent": agent,
        "budget": {"type": "limit", "max_list_cost": {"amount": "100", "currency": "USD"}},
        "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
    }))
    .unwrap()
}

async fn assert_no_root(repo: &SqliteManagedSessionRepository) {
    assert!(
        repo.scan(SessionScanQuery::Workspace {
            workspace_id: awaken_tenancy::DEFAULT_WORKSPACE_ID
        })
        .await
        .unwrap()
        .into_sessions()
        .is_empty(),
        "refusal must precede root insertion"
    );
}

#[tokio::test]
async fn managed_skill_refusal_precedes_bare_capability_capture() {
    // R2: no profile + advertised child + one valid authored Skill. Inject an
    // original RunError through the actual Environment Skill port. Skill error
    // => exact error, zero child lookups, zero later price requests, no root.
    // Neutral's empty success is not a refusal and supplies no such evidence.
    let repo = support::session_repository("Skill/capture repository").await;
    let profiles = Arc::new(ObservedBudgetProfiles::new(3));
    let prices = Arc::new(GatedPrices::new(None));
    let runtime = RehydrateFake::default();
    *runtime.delegate_ids.lock() = vec!["budget-child".into()];
    let mut roles = selected_roles(runtime);
    let awaken_session_execution_contract::SessionPlacementRoles::Colocated(placement) =
        &mut roles.placement
    else {
        panic!("R2 retains the original co-located placement")
    };
    placement.environment = Arc::new(RefusingSkills {
        original: placement.environment.clone(),
    });
    let managed = state(roles, repo.clone(), profiles.clone(), prices.clone());
    let request = budget_request(json!({"type":"agent_with_overrides", "id":"bare-runtime",
        "skills":[{"type":"custom", "skill_id":"selected-skill", "version":"1"}]}));
    let result = tokio::time::timeout(
        WAIT,
        managed.create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID),
    )
    .await
    .expect("R2 the original Skill refusal completes");
    let awaken_protocol_managed::StateError::Run(error) = result.unwrap_err() else {
        panic!("R2 original RunError")
    };
    assert_eq!(error.kind, RunErrorKind::Internal);
    assert_eq!(error.code, "capture_test_skill_refused");
    assert_eq!(error.message, "original Skill port refused selection");
    assert!(
        profiles.child_reads().is_empty(),
        "R2 capture stays after Skill resolution"
    );
    assert!(prices.requests.lock().unwrap().is_empty());
    assert_no_root(repo.as_ref()).await;
}

#[tokio::test]
async fn managed_bare_capability_values_survive_a_later_catalog_change() {
    // R3: no root profile, current child/default/repeated child, followed by
    // an actual suspended list-price await. Capture current4, change catalog to5
    // only while that later await is entered, then finish creation. Accepted
    // configuration must remain [4,default1,4], ordered, with one current query
    // per declared occurrence and no lookup at the final roster compiler.
    let repo = support::session_repository("bare capability repository").await;
    let profiles = Arc::new(ObservedBudgetProfiles::new(3));
    let prices = Arc::new(GatedPrices::new(None));
    let runtime = RehydrateFake::default();
    *runtime.delegate_ids.lock() = vec![
        "budget-child".into(),
        "missing".into(),
        "budget-child".into(),
    ];
    let managed = state(
        selected_roles(runtime),
        repo.clone(),
        profiles.clone(),
        prices.clone(),
    );
    let creation = tokio::spawn(async move {
        managed
            .create_session(
                budget_request(json!("bare-runtime")),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
    });
    prices.wait_for_entry().await;
    let captured_reads = vec![
        ("budget-child".into(), None),
        ("missing".into(), None),
        ("budget-child".into(), None),
    ];
    assert_eq!(
        profiles.child_reads(),
        captured_reads,
        "R3 capture precedes the later await"
    );
    profiles.current_revision.store(5, Ordering::SeqCst);
    prices.release.notify_one();
    let accepted = tokio::time::timeout(WAIT, creation)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let children = &accepted.agent.multiagent.as_ref().unwrap().agents;
    let values = children
        .iter()
        .map(|member| {
            let awaken_protocol_managed::types::SessionMultiagentRosterEntry::Agent(child) = member
            else {
                panic!("R3 ordinary capability child")
            };
            (child.name.as_str(), child.version)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        [
            ("budget-child-v4", 4),
            ("missing", 1),
            ("budget-child-v4", 4)
        ]
    );
    assert_eq!(
        profiles.child_reads(),
        captured_reads,
        "R3 no late re-selection"
    );
    let root = repo.get(&SessionId::from(&accepted.id)).await.unwrap();
    assert_eq!(
        SessionUpdateHistory::agent_configuration(&root)
            .unwrap()
            .multiagent
            .unwrap()
            .members
            .len(),
        3
    );
    assert_eq!(prices.original.model_rosters.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn managed_malformed_exact_profile_keeps_its_late_refusal_order() {
    // R4/R5: requested child3 returns a profile at4, but the original exact
    // executable snapshots and geography remain valid. SessionQueries does NOT
    // validate the response revision. Capture must therefore pass through the
    // original later price await before the unchanged final compiler refuses.
    // R4 price success => original exact mismatch; R5 price Unavailable => that
    // earlier error wins. Both retain no durable root. Advancing the whole
    // roster compiler to capture time would fail to enter this price boundary.
    for refusal in [
        None,
        Some(ManagedListPriceError::Unavailable(
            "original price source refused".into(),
        )),
    ] {
        let repo = support::session_repository("late exact refusal repository").await;
        let profiles = Arc::new(ObservedBudgetProfiles::new(4));
        let prices = Arc::new(GatedPrices::new(refusal.clone()));
        let managed = state(
            selected_roles(RehydrateFake::default()),
            repo.clone(),
            profiles.clone(),
            prices.clone(),
        );
        let creation = tokio::spawn(async move {
            managed
                .create_session(
                    budget_request(json!({"type":"agent", "id":"budget-root", "version":7})),
                    awaken_tenancy::DEFAULT_WORKSPACE_ID,
                )
                .await
        });
        prices.wait_for_entry().await;
        assert_eq!(profiles.child_reads(), [("budget-child".into(), Some(3))]);
        assert_no_root(repo.as_ref()).await;
        prices.release.notify_one();
        let result = tokio::time::timeout(WAIT, creation).await.unwrap().unwrap();
        let awaken_protocol_managed::StateError::Run(error) = result.unwrap_err() else {
            panic!("R4/R5 original RunError")
        };
        if let Some(refusal) = refusal {
            assert_eq!(
                error.kind,
                RunErrorKind::Unavailable,
                "R5 price error precedes roster rejection"
            );
            assert_eq!(error.code, "unavailable");
            assert_eq!(error.message, refusal.to_string());
            assert!(prices.original.model_rosters.lock().unwrap().is_empty());
        } else {
            assert_eq!(error.kind, RunErrorKind::BadRequest);
            assert_eq!(error.code, "invalid_request");
            assert_eq!(
                error.message,
                "multiagent_unavailable: Agent `budget-child` publication revision Some(3) is unavailable"
            );
            assert_eq!(prices.original.model_rosters.lock().unwrap().len(), 1);
        }
        assert_no_root(repo.as_ref()).await;
    }
}
