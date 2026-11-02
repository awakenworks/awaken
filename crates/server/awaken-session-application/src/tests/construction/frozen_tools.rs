//! T06-C03 (冻结与动态事实), frozen tool settings: an admitted Session's tool
//! configuration was compiled once, at admission. A later, stricter tool
//! compiler is the CURRENT authority for new authoring, but must not
//! re-decide the frozen settings of an unrelated command.
//!
//! The update compiler judges explicitly authored tools and preserves omitted
//! tools. These tests pin both sides: an unrelated title update succeeds with
//! the admitted settings unchanged, while new legacy authoring is refused.
use super::*;
use awaken_agent_contract::{
    ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
};
use awaken_tool_contract::{
    ToolConfigurationCompiler, ToolConfigurationError, ToolConfigurationInput,
};

/// `strict == false`: the compiler that admitted the Session. `strict == true`:
/// the current compiler, which no longer accepts the `legacy` setting.
struct VersionedCompiler {
    strict: bool,
}

impl ToolConfigurationCompiler for VersionedCompiler {
    fn compile(
        &self,
        input: ToolConfigurationInput<'_>,
    ) -> Result<serde_json::Value, ToolConfigurationError> {
        if self.strict && input.configuration.get("legacy").is_some() {
            return Err(ToolConfigurationError {
                path: "legacy".into(),
                code: "legacy_setting_retired".into(),
                message: "the current compiler retired this setting".into(),
            });
        }
        Ok(input.configuration.clone())
    }
}

fn legacy_tools() -> awaken_session_configuration_contract::SessionToolConfiguration {
    awaken_session_configuration_contract::SessionToolConfiguration {
        toolsets: vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy::default(),
            overrides: vec![ToolPolicyOverride::with_optional_configuration(
                "web_fetch",
                ToolExecutionPolicy::default(),
                Some(serde_json::json!({"legacy": true})),
            )],
        }],
        client_tools: vec![],
    }
}

fn app(
    repository: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    strict: bool,
) -> SessionApplication {
    let executor = SessionExecutor::colocated(
        Arc::new(creation::RecordingCreateRuntime::default()),
        Arc::new(NoopMcpRealizer),
    );
    // This fixture has committed no Thread. Select its existing empty read
    // role before construction; the general creation-failure fixture keeps
    // its intentional unsupported-read behavior for cleanup admission tests.
    let roles = awaken_session_execution_contract::SessionExecutionRoles {
        thread_reads: Arc::new(NoopRuntime),
        ..(*executor).clone()
    };
    SessionApplicationBuilder::new(
        roles,
        SessionPersistence::new(repository, system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        {
            let probe = std::sync::Arc::new(crate::ExecutionObservationProbe::default());
            crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe)
        },
        Arc::new(VersionedCompiler { strict }),
    )
    .build()
    .expect("fixture application")
}

fn title_only(title: &str) -> SessionUpdateCommand {
    SessionUpdateCommand {
        title: Some(SessionFieldUpdate::Replace(title.into())),
        metadata: None,
        budget: None,
        tools: None,
        mcp_update: None,
        idempotency_key: None,
        request_fingerprint: format!("title-only-{title}"),
        if_match: None,
    }
}

async fn admitted_under_the_old_compiler(
    id: &str,
) -> (
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
    PersistedSession,
) {
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("frozen tools repository"),
    );
    let mut command = creation::creation_command(id);
    command.tools = legacy_tools();
    command.intent.control.toolsets = legacy_tools().toolsets;
    let created = app(repository.clone(), false)
        .creation_commands
        .create_session(command)
        .await
        .expect("admitted by the compiler of its time");
    assert_eq!(created.tools, legacy_tools(), "the admitted settings");
    (repository, created)
}

#[tokio::test]
async fn a_stricter_current_compiler_never_rewrites_frozen_tool_settings() {
    let (repository, created) = admitted_under_the_old_compiler("frozen-tools-kept").await;
    let current = app(repository.clone(), true);
    let _ = current
        .update_commands
        .update_session(&created.session_id, title_only("later"))
        .await;
    let after = repository.get(&created.session_id).await.unwrap();
    assert_eq!(
        after.tools,
        legacy_tools(),
        "O0 frozen tool settings unchanged"
    );
    assert_eq!(
        after.frozen_baseline().unwrap().toolsets,
        legacy_tools().toolsets,
        "O0 frozen baseline toolsets unchanged"
    );
    // The current authority still refuses NEW authoring of the retired setting.
    let refused = current
        .update_commands
        .update_session(
            &created.session_id,
            SessionUpdateCommand {
                tools: Some(legacy_tools()),
                ..title_only("re-author")
            },
        )
        .await;
    let error = match refused {
        Err(crate::SessionUpdateError::Rejected(error)) => error,
        other => panic!("new authoring is judged by the current compiler: {other:?}"),
    };
    assert_eq!(
        error.code, "legacy_setting_retired",
        "current authority for new authoring"
    );
}

#[tokio::test]
async fn a_title_only_update_does_not_revalidate_frozen_tool_settings() {
    // DT-T: C1 an older compiler admitted these frozen settings; C2 today's
    // compiler refuses new authoring of the same settings; C3 tools omitted
    // during a title update. T1 C1+C2+C3 => title commits, frozen tools remain
    // byte-equal. The sibling explicit-authoring row independently proves C2;
    // this test reuses the existing omission branch, adding no alternate compiler.
    let (repository, created) = admitted_under_the_old_compiler("frozen-tools-title").await;
    let current = app(repository.clone(), true);
    let outcome = current
        .update_commands
        .update_session(&created.session_id, title_only("later"))
        .await
        .expect("an unrelated title update is not refused by a later tool compiler");
    assert!(outcome.command_applied);
    let after = repository.get(&created.session_id).await.unwrap();
    assert_eq!(after.title.as_deref(), Some("later"));
    assert_eq!(
        after.tools,
        legacy_tools(),
        "frozen tool settings unchanged"
    );
}
