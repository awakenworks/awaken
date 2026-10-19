//! Real Managed launch admission with controlled upstream read results.
use super::*;

// This dependency double is necessary because the real in-memory catalog has
// no public typed failure injection. It owns only a supplied result and read
// coordinates, never an Environment, registration policy, queue or classifier.
// Existing AdmissionEnvironment.get(None) and RecordingEnvironmentSource's
// unreachable get remain unchanged. All non-selected reads reject accidental
// use, including the Agent profile source that must follow a successful D1.
struct DeploymentReadProbe {
    response: Result<
        Option<awaken_executable_environment_contract::ExecutableEnvironmentRegistration>,
        awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
    >,
    environment_reads: std::sync::Mutex<Vec<String>>,
    profile_reads: std::sync::atomic::AtomicUsize,
}

impl DeploymentReadProbe {
    fn unexpected_profile_read(&self) -> ! {
        self.profile_reads.fetch_add(1, Ordering::SeqCst);
        panic!("D1 Environment refusal must precede every Agent profile read")
    }
}

#[async_trait::async_trait]
impl awaken_executable_environment_contract::ExecutableEnvironmentLifecycleSource
    for DeploymentReadProbe
{
    async fn current_lifecycle(
        &self,
        _environment_id: &str,
    ) -> Result<
        Option<awaken_executable_environment_contract::ExecutableEnvironmentLifecycle>,
        awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
    > {
        unreachable!("D1 launch uses the selected current registration only")
    }
}

#[async_trait::async_trait]
impl awaken_executable_environment_contract::ExecutableEnvironmentRegistrationSource
    for DeploymentReadProbe
{
    async fn current_registration(
        &self,
        environment_id: &str,
    ) -> Result<
        Option<awaken_executable_environment_contract::ExecutableEnvironmentRegistration>,
        awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
    > {
        self.environment_reads
            .lock()
            .unwrap()
            .push(environment_id.to_owned());
        self.response.clone()
    }

    async fn current_registrations(
        &self,
    ) -> Result<
        Vec<awaken_executable_environment_contract::ExecutableEnvironmentRegistration>,
        awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
    > {
        unreachable!("D1 launch must not turn a selected read into a catalog scan")
    }

    async fn registration_at_revision(
        &self,
        _environment_id: &str,
        _revision: awaken_environment_contract::EnvironmentRevision,
    ) -> Result<
        Option<awaken_executable_environment_contract::ExecutableEnvironmentRegistration>,
        awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
    > {
        unreachable!("D1 launch does not resolve a Session publication")
    }
}

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for DeploymentReadProbe {
    fn session_profile_in(
        &self,
        _workspace: &str,
        _agent: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.unexpected_profile_read()
    }

    fn agent_unavailable_in(&self, _workspace: &str, _agent: &str) -> bool {
        self.unexpected_profile_read()
    }

    fn session_profile_at_revision_in(
        &self,
        _workspace: &str,
        _agent: &str,
        _revision: u64,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.unexpected_profile_read()
    }

    fn executable_snapshot_at_revision_in(
        &self,
        _workspace: &str,
        _agent: &str,
        _revision: u64,
    ) -> Option<ExecutableAgentSnapshot> {
        self.unexpected_profile_read()
    }

    fn unavailable_delegate_in(&self, _workspace: &str, _agent: &str) -> Option<String> {
        self.unexpected_profile_read()
    }
}

#[tokio::test]
async fn nonlocal_environment_refusals_precede_agent_reads_and_session_creation() {
    // D1 decision table: valid nonlocal target + current registration None ->
    // original EnvironmentNotFound failure; typed upstream Err -> original
    // Unavailable text. Both must call the real catalog inlet once with the
    // exact id, and then perform zero Agent reads and zero Session creates.
    // D0 inputs are otherwise valid; the same original Environment execution
    // app, Session application, Managed launcher, Store and Host clock execute
    // both rows. The fixture supplies no launch decision or wire classifier.
    use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
    for (rule, response) in [
        ("D1-none", Ok(None)),
        (
            "D1-error",
            Err(ExecutableEnvironmentRegistrationError::Unavailable(
                "typed catalog outage".into(),
            )),
        ),
    ] {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let (host, resources) = build_unmounted_platform_with_agent_publications(
            Arc::new(EchoModel),
            "claude-sonnet-5",
            catalog,
        )
        .await;
        let clock = host.clock();
        let workspace_id = host.local_workspace().to_string();
        let source = Arc::new(DeploymentReadProbe {
            response,
            environment_reads: Default::default(),
            profile_reads: 0.into(),
        });
        let environments = Arc::new(
            awaken_environment_application::execution::EnvironmentExecutionApplication::new(
                Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock)),
                source.clone(),
            ),
        );
        let managed = awaken_coordinator::local_managed_state_with_environments_and_agent_source(
            host,
            resources.authorities().resource_registry(),
            environments,
            source.clone(),
        )
        .await;
        let launcher = ManagedDeploymentSessionLauncher::new(managed.managed.clone());
        let outcome = launcher
            .launch(DeploymentLaunch {
                deployment_id: "depl_catalog_refusal".into(),
                deployment_run_id: format!("drun_catalog_{rule}"),
                trigger: Some(awaken_deployment_application::DeploymentTrigger::Manual),
                timezone: None,
                workspace_id: workspace_id.clone(),
                agent: DeploymentAgent::new("assistant", 1),
                environment_id: "env_nonlocal".into(),
                metadata: Default::default(),
                initial_events: vec![DeploymentSeedEvent::UserMessage {
                    content: vec![ContentBlock::text("not admitted")],
                }],
                resources: Vec::new(),
                vault_ids: Vec::new(),
                budget_max_list_cost_minor: None,
            })
            .await;
        match (rule, outcome) {
            (
                "D1-none",
                DeploymentLaunchOutcome::Failed {
                    error:
                        awaken_deployment_application::DeploymentRunFailure::EnvironmentNotFoundError {
                            message,
                        },
                },
            ) => assert_eq!(
                message, "environment `env_nonlocal` no longer exists",
                "D1-none"
            ),
            ("D1-error", DeploymentLaunchOutcome::Unavailable { message }) => assert_eq!(
                message,
                "environment catalog is unavailable while resolving `env_nonlocal`: executable Environment registration unavailable: typed catalog outage",
                "D1-error",
            ),
            (_, outcome) => {
                panic!("{rule} returned an unexpected real launch outcome: {outcome:?}")
            }
        }
        assert_eq!(
            *source.environment_reads.lock().unwrap(),
            vec!["env_nonlocal".to_owned()],
            "{rule} one exact catalog call"
        );
        assert_eq!(
            source.profile_reads.load(Ordering::SeqCst),
            0,
            "{rule} no later profile read"
        );
        let scan = managed
            .sessions
            .session_repository_handle()
            .scan(awaken_session_state_contract::SessionScanQuery::Workspace {
                workspace_id: &workspace_id,
            })
            .await
            .unwrap();
        assert!(
            scan.sessions.is_empty() && scan.quarantined.is_empty() && scan.next_cursor.is_none(),
            "{rule} no Session root or incomplete read"
        );
    }
}
