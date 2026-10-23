//! Selected profile-port identity, absence and call-time value observations.
use super::*;

type ProfileSourceCall = (&'static str, String, String, Option<u64>);

// The original static ProfiledAgent/SubstitutingProfileSource remain the value
// producers. Neither can change its selected inner source or observe callbacks;
// this test dependency supplies only that handle and typed invocation evidence.
// It creates no catalog, revision matcher, snapshot or Session implementation.
struct SelectedProfileProbe {
    inner: Mutex<Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>>,
    calls: Mutex<Vec<ProfileSourceCall>>,
    delegate_marker: &'static str,
}

impl SelectedProfileProbe {
    fn record(&self, method: &'static str, workspace: &str, agent: &str, revision: Option<u64>) {
        self.calls
            .lock()
            .unwrap()
            .push((method, workspace.to_owned(), agent.to_owned(), revision));
    }

    // Return an owned Arc: this method's MutexGuard is gone before any caller
    // invokes the selected source, as with Flow's call-time control handle.
    fn current(&self) -> Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource> {
        self.inner.lock().unwrap().clone()
    }
}

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for SelectedProfileProbe {
    fn session_profile_in(
        &self,
        workspace: &str,
        agent: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        self.record("current-profile", workspace, agent, None);
        self.current().session_profile_in(workspace, agent)
    }

    fn session_profile_at_revision_in(
        &self,
        workspace: &str,
        agent: &str,
        revision: u64,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        self.record("exact-profile", workspace, agent, Some(revision));
        self.current()
            .session_profile_at_revision_in(workspace, agent, revision)
    }

    fn executable_snapshot_at_revision_in(
        &self,
        workspace: &str,
        agent: &str,
        revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        self.record("exact-snapshot", workspace, agent, Some(revision));
        self.current()
            .executable_snapshot_at_revision_in(workspace, agent, revision)
    }

    fn agent_unavailable_in(&self, workspace: &str, agent: &str) -> bool {
        self.record("agent-unavailable", workspace, agent, None);
        self.current().agent_unavailable_in(workspace, agent)
    }

    fn unavailable_delegate_in(&self, workspace: &str, agent: &str) -> Option<String> {
        self.record("custom-delegate", workspace, agent, None);
        Some(self.delegate_marker.to_owned())
    }
}

#[tokio::test]
async fn selected_profile_port_keeps_absent_and_missing_snapshot_budget_refusals() {
    // P1-P3 cause/effect table at the real App budget-roster query:
    // P1 no Source + no root profile -> legacy Native model, no source refusal.
    // P2 no Source + original pinned9 profile -> missing-publication9 refusal.
    // P3 Source present + original profile9 but snapshot9 absent -> SAME original
    // missing-publication9 refusal, before the intentionally invalid fallback
    // backend/model can win. Source-presence and profile-vs-snapshot assertions
    // below only prove selector preconditions; the App call owns error ordering.
    // This query writes no Session, prepares no Runtime and dispatches no Work;
    // original create/budget B1-B11 cases separately own admission/CAS behavior.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(RecordingCreateRuntime::default());
    let environment = Arc::new(RecordingEnvironmentSource::default());
    let mut app =
        application_with_runtime(runtime.clone(), repository.clone(), environment.clone());
    assert!(
        !app.queries.has_agent_profile_source(),
        "P1/P2 no selected source"
    );
    assert_eq!(
        app.creation_commands
            .managed_session_model_refs(
                "workspace",
                "profiled",
                None,
                "legacy-model",
                Some("native"),
                None
            )
            .unwrap(),
        ["legacy-model"],
        "P1 no-profile return precedes the required-source branch"
    );
    // P1b: a legitimate unversioned profile0 with a declared child and no
    // Advisor reaches the original late source requirement after Native/model
    // admission. The child's missing revision is deliberately a later refusal;
    // SourceNone must win first. The P1-P3 zero-effects/root checks below are
    // shared by this additional row as well.
    let mut legacy =
        awaken_executable_agent_contract::ExecutableAgentProfileSource::session_profile_in(
            &ProfiledAgent { unavailable: false },
            "workspace",
            "profiled",
        )
        .unwrap();
    assert_eq!(
        legacy.source_revision, 0,
        "P1b original unversioned profile"
    );
    assert!(
        legacy.advisor_model.is_none(),
        "P1b no earlier Advisor refusal"
    );
    legacy
        .delegates
        .push(awaken_executable_agent_contract::ExecutableAgentDelegate {
            agent_id: "legacy-child".into(),
            source_revision: None,
        });
    let error = app
        .creation_commands
        .managed_session_model_refs(
            "workspace",
            "profiled",
            Some(&legacy),
            "legacy-model",
            Some("native"),
            None,
        )
        .expect_err("P1b late required source");
    assert_eq!(
        error.kind,
        awaken_session_types::RunErrorKind::BadRequest,
        "P1b"
    );
    assert_eq!(
        error.message, "budget_price_roster_unavailable: Agent publication source is unavailable",
        "P1b source absence precedes child revision validation"
    );
    let profile9 = awaken_executable_agent_contract::ExecutableAgentProfileSource::session_profile_at_revision_in(
        &ProfiledAgent { unavailable: false }, "workspace", "profiled", 9,
    ).expect("original profile9 exists without a complete snapshot");
    for (rule, present) in [("P2", false), ("P3", true)] {
        if present {
            app.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
            assert!(app.queries.has_agent_profile_source(), "P3");
            assert!(
                app.queries
                    .selected_session_profile("workspace", "profiled", Some(9))
                    .is_some(),
                "P3 profile9"
            );
        }
        assert!(
            app.queries
                .executable_snapshot_at_revision("workspace", "profiled", 9)
                .is_none(),
            "{rule} snapshot9"
        );
        let error = app
            .creation_commands
            .managed_session_model_refs(
                "workspace",
                "profiled",
                Some(&profile9),
                "",
                Some("acp:later-refusal"),
                None,
            )
            .expect_err(rule);
        assert_eq!(
            error.kind,
            awaken_session_types::RunErrorKind::BadRequest,
            "{rule}"
        );
        assert_eq!(
            error.message,
            "budget_price_roster_unavailable: Agent `profiled` revision 9 has no executable publication",
            "{rule}"
        );
    }
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0, "P1-P3");
    assert_eq!(
        environment.dispatch_calls.load(Ordering::SeqCst),
        0,
        "P1-P3"
    );
    let scan = repository
        .scan(awaken_session_state_contract::SessionScanQuery::Workspace {
            workspace_id: "workspace",
        })
        .await
        .unwrap();
    assert!(
        scan.sessions.is_empty() && scan.quarantined.is_empty() && scan.next_cursor.is_none(),
        "P1-P3 complete empty root scan"
    );
}

#[tokio::test]
async fn selected_profile_port_reads_the_live_handle_and_custom_delegate_without_effects() {
    // P4-P7: constructor-selected HANDLE, zero constructor callbacks -> no value
    // prefetch/cache. An actually created exact7 Session first projects the
    // original full snapshot. Changing only that SAME handle's inner source to
    // original SubstitutingProfileSource -> the next frozen projection sees
    // snapshot absence; no new App/Queries/source setter/Runtime is installed.
    // A custom delegate marker deliberately differs from ProfiledAgent's
    // default(None); the existing Deployment role must invoke that override,
    // not copy the default profile traversal. Expected values come from the
    // old sources/literals, never the new Query method compared with itself.
    // Exact-profile8 after the switch is supplementary call-routing evidence,
    // not proof that a mismatched publication is admitted. Original requested
    // revision tests still reject it. Frozen reads change no root or effects.
    // The ORIGINAL AdmissionEnvironment supplies all three Environment roles:
    // it resolves high-level Session inputs; its enqueue/wake/retire callbacks
    // are fail-fast for any unexpected Work selection. This is the injected
    // Work oracle, not a count from an unused RecordingEnvironmentSource.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(RecordingCreateRuntime::default());
    let environment = Arc::new(AdmissionEnvironment);
    let source = Arc::new(SelectedProfileProbe {
        inner: Mutex::new(Arc::new(ProfiledAgent { unavailable: false })),
        calls: Mutex::default(),
        delegate_marker: "custom-delegate-marker",
    });
    let app = SessionApplicationBuilder::fixture(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        SessionPersistence::new(repository.clone(), system_clock()),
        environment.clone(),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .agent_profiles(source.clone())
    .build()
    .unwrap();
    assert!(
        source.calls.lock().unwrap().is_empty(),
        "P4 construction has no catalog callback"
    );
    let mut command = profiled_session_command("selected-profile-live", None);
    command.source_revision = Some(7);
    let session = app
        .creation_commands
        .create_profiled_session(command)
        .await
        .unwrap();
    let before = repository.get(&session.session_id).await.unwrap();
    let preparations = runtime.preparations.load(Ordering::SeqCst);
    let start = source.calls.lock().unwrap().len();
    let expected = awaken_executable_agent_contract::ExecutableAgentProfileSource::executable_snapshot_at_revision_in(
        &ProfiledAgent { unavailable: false }, "workspace", "profiled", 7,
    ).unwrap();
    let first = app
        .queries()
        .frozen_session_projection("workspace".into(), &session, false)
        .await
        .unwrap();
    assert_eq!(
        first.agent_publication.as_ref(),
        Some(&expected),
        "P5 original complete exact7"
    );
    assert_eq!(
        awaken_executable_agent_contract::ExecutableAgentProfileSource::unavailable_delegate_in(
            &ProfiledAgent { unavailable: false },
            "workspace",
            "profiled"
        ),
        None,
        "P6 independent default-path counterexample"
    );
    assert_eq!(
        crate::DeploymentSessionEnvironment::deployment_unavailable_delegate(
            &app,
            "workspace",
            "profiled"
        ),
        Some("custom-delegate-marker".into()),
        "P6 original source override"
    );
    *source.inner.lock().unwrap() = Arc::new(SubstitutingProfileSource);
    let next = app
        .queries()
        .frozen_session_projection("workspace".into(), &session, false)
        .await
        .unwrap();
    assert!(
        next.agent_publication.is_none(),
        "P7 same handle's new inner is not constructor cached"
    );
    assert_eq!(
        app.queries
            .selected_session_profile("workspace", "profiled", Some(7))
            .unwrap()
            .source_revision,
        8,
        "P7 original substituted value, not an admission"
    );
    assert_eq!(
        &source.calls.lock().unwrap()[start..],
        &[
            (
                "exact-snapshot",
                "workspace".into(),
                "profiled".into(),
                Some(7)
            ),
            (
                "custom-delegate",
                "workspace".into(),
                "profiled".into(),
                None
            ),
            (
                "exact-snapshot",
                "workspace".into(),
                "profiled".into(),
                Some(7)
            ),
            (
                "exact-profile",
                "workspace".into(),
                "profiled".into(),
                Some(7)
            ),
        ],
        "P5-P7 exact callback order and coordinates"
    );
    let absent = application_with_runtime(runtime.clone(), repository.clone(), environment.clone());
    assert_unconfigured_roster_reads_the_root_first(&absent, &session).await;
    assert_eq!(
        runtime.preparations.load(Ordering::SeqCst),
        preparations,
        "P5-P7"
    );
    assert_eq!(
        repository.get(&session.session_id).await.unwrap(),
        before,
        "P5-P7 exact root unchanged"
    );
}

// P8: same actually created active exact7 Session and repository, but the
// original source-absent fixture application. The real coordination query
// rejects at root-profile lookup before its defensive late source guard.
// Under the one immutable holder, absent source cannot first produce a root
// profile, so that later absence branch is unreachable; no divergent holders
// are fabricated to pretend otherwise. The caller retains its original shared
// exact-root/preparation assertions after this read; the same injected
// AdmissionEnvironment rejects any unexpected enqueue/wake/retire request.
async fn assert_unconfigured_roster_reads_the_root_first(
    app: &SessionApplication,
    session: &PersistedSession,
) {
    assert!(
        !app.queries.has_agent_profile_source(),
        "P8 source-absent fixture"
    );
    assert!(
        session.execution.admits_activity()
            && !session.is_terminal()
            && !session.terminal_cleanup.is_fenced(),
        "P8 real active Session reaches the roster query"
    );
    let error = awaken_session_execution_contract::SessionAgentCoordination::list_session_agents(
        app,
        &session.session_id,
    )
    .await
    .expect_err("P8 original root-profile refusal");
    assert_eq!(
        error.kind,
        awaken_session_types::RunErrorKind::Unavailable,
        "P8"
    );
    assert_eq!(
        error.message, "the Session's frozen coordinator Agent `profiled` is unavailable",
        "P8 root lookup precedes the late source defense"
    );
}
