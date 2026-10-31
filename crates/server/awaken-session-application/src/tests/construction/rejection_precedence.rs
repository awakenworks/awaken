//! T04-C01 (早期拒绝): an invalid command returns the contract's FIRST error,
//! classified by its stable code, before any preparation, root write or model
//! effect.
//!
//! Oracles (groups JSON order):
//! - O0 the contract's first error is returned;
//! - O1 later preparation, root writes and model calls stay at zero.
//!
//! Precedence under test (production order): create compiles the inherited
//! intent toolsets, then finalizes the intent, then compiles the explicit tool
//! settings (creation.rs:538-555), all before the sandbox layout and the root
//! insert (creation.rs:707-788). Update refuses a stale `If-Match` before it
//! compiles tool settings (update.rs:320-335).
//!
//! Negative control: "rewording an unrelated error message must not change the
//! classification or the precedence". It is executed: every row runs under two
//! compiler wordings. The second wording embeds the OTHER tool's code in the
//! first error's message, so a message-text classifier misreads it, while the
//! typed `(kind, code)` answer and the precedence stay identical.
//!
//! | Row | Inherited toolsets | Explicit tools | Command | Expected code |
//! |---|---|---|---|---|
//! | P1 | rejected (web_fetch) | rejected (web_search) | create | web_fetch_rejected |
//! | P2 | valid | rejected (web_search) | create | web_search_rejected |
//! | P3 | rejected (web_fetch) | rejected (web_search) | accept (durable only) | web_fetch_rejected |
//! | U1 | - | rejected | update with stale If-Match | Conflict |
//! | U2 | - | rejected | update | web_search_rejected |
use super::*;
use awaken_agent_contract::{
    ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
};
use awaken_tool_contract::{
    ToolConfigurationCompiler, ToolConfigurationError, ToolConfigurationInput,
};

const FIRST: &str = "web_fetch";
const SECOND: &str = "web_search";

/// A collaborator compiler that rejects any setting marked `reject`, with a
/// stable code and a configurable human message.
struct ScriptedCompiler {
    wording: &'static str,
}

impl ToolConfigurationCompiler for ScriptedCompiler {
    fn compile(
        &self,
        input: ToolConfigurationInput<'_>,
    ) -> Result<serde_json::Value, ToolConfigurationError> {
        if input.configuration.get("reject") == Some(&serde_json::Value::Bool(true)) {
            return Err(ToolConfigurationError {
                path: String::new(),
                code: format!("{}_rejected", input.tool_name),
                message: format!("{} {}", self.wording, input.tool_name),
            });
        }
        Ok(input.configuration.clone())
    }
}

fn rejected(tool: &str) -> Vec<ToolsetPolicy> {
    vec![ToolsetPolicy {
        source: ToolsetSource::Agent,
        default: ToolExecutionPolicy::default(),
        overrides: vec![ToolPolicyOverride::with_optional_configuration(
            tool,
            ToolExecutionPolicy::default(),
            Some(serde_json::json!({"reject": true})),
        )],
    }]
}

/// The mutant classifier the NC refutes: branch on message text.
fn message_classifier(error: &RunError) -> &'static str {
    let text = error.to_string();
    if text.contains("web_search_rejected") {
        "web_search_rejected"
    } else if text.contains("web_fetch_rejected") {
        "web_fetch_rejected"
    } else {
        "unclassified"
    }
}

struct Fixture {
    repository: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    runtime: Arc<creation::RecordingCreateRuntime>,
    environment: Arc<RecordingEnvironmentSource>,
    app: SessionApplication,
}

async fn fixture(wording: &'static str) -> Fixture {
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("precedence repository"),
    );
    let runtime = Arc::new(creation::RecordingCreateRuntime::default());
    let environment = Arc::new(RecordingEnvironmentSource::default());
    let app = SessionApplicationBuilder::fixture(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        SessionPersistence::new(repository.clone(), system_clock()),
        environment.clone(),
        Arc::new(ScriptedCompiler { wording }),
    )
    .build()
    .expect("fixture application");
    Fixture {
        repository,
        runtime,
        environment,
        app,
    }
}

impl Fixture {
    fn effects(&self) -> (usize, usize, usize) {
        (
            self.runtime.preparations.load(Ordering::SeqCst),
            self.runtime.baseline_installs.load(Ordering::SeqCst),
            self.environment.dispatch_calls.load(Ordering::SeqCst),
        )
    }
}

/// One rejected creation row: returns the typed (kind, code) and the error.
async fn rejected_create(
    fixture: &Fixture,
    row: &str,
    inherited: bool,
    durable_only: bool,
) -> RunError {
    let id = format!("precedence-{row}");
    let mut command = creation::creation_command(&id);
    if inherited {
        command.intent.control.toolsets = rejected(FIRST);
    }
    command.tools = awaken_session_configuration_contract::SessionToolConfiguration {
        toolsets: rejected(SECOND),
        client_tools: vec![],
    };
    let effects = fixture.effects();
    let result = if durable_only {
        fixture.app.creation_commands.accept_session(command).await
    } else {
        fixture.app.creation_commands.create_session(command).await
    };
    let error = match result {
        Err(SessionCreationError::Rejected(error)) => error,
        other => panic!("{row} must be rejected: {other:?}"),
    };
    assert!(
        fixture.repository.get(&SessionId::from(id)).await.is_err(),
        "{row}/O1 no durable root"
    );
    assert_eq!(
        fixture.effects(),
        effects,
        "{row}/O1 no preparation or dispatch"
    );
    error
}

#[tokio::test]
async fn invalid_commands_return_the_contracted_first_error_without_effects() {
    let mut answers = Vec::new();
    for wording in ["rejected", "unlike web_search_rejected, rejected"] {
        let fixture = fixture(wording).await;
        let mut typed = Vec::new();
        for (row, inherited, durable_only, expected) in [
            ("P1", true, false, "web_fetch_rejected"),
            ("P2", false, false, "web_search_rejected"),
            ("P3", true, true, "web_fetch_rejected"),
        ] {
            let error = rejected_create(&fixture, row, inherited, durable_only).await;
            assert_eq!(
                (error.kind, error.code.as_str()),
                (awaken_session_types::RunErrorKind::BadRequest, expected),
                "{row}/O0 typed first error under wording {wording:?}"
            );
            typed.push((row, error.kind, error.code.clone()));
            if row == "P1" && wording != "rejected" {
                // NC: the rewording misleads a text classifier only.
                assert_ne!(
                    message_classifier(&error),
                    expected,
                    "NC the reworded message would flip a text-based classification"
                );
            }
        }
        assert_eq!(
            fixture.effects(),
            (0, 0, 0),
            "O1 zero effects for every row"
        );

        // U1/U2: update precedence over a created Session.
        let created = fixture
            .app
            .creation_commands
            .create_session(creation::creation_command("precedence-update"))
            .await
            .expect("valid Session");
        let before = fixture.repository.get(&created.session_id).await.unwrap();
        let effects = fixture.effects();
        let update = |if_match| SessionUpdateCommand {
            title: Some(SessionFieldUpdate::Replace("must not persist".into())),
            metadata: None,
            budget: None,
            tools: Some(
                awaken_session_configuration_contract::SessionToolConfiguration {
                    toolsets: rejected(SECOND),
                    client_tools: vec![],
                },
            ),
            mcp_update: None,
            idempotency_key: None,
            request_fingerprint: "precedence-update".into(),
            if_match,
        };
        let stale = awaken_session_types::SessionRevision(before.revision.0 + 7);
        let conflict = fixture
            .app
            .update_commands
            .update_session(&created.session_id, update(Some(stale)))
            .await;
        assert!(
            matches!(conflict, Err(crate::SessionUpdateError::Conflict)),
            "U1/O0 a stale If-Match precedes tool compilation: {conflict:?}"
        );
        let rejected_update = fixture
            .app
            .update_commands
            .update_session(&created.session_id, update(None))
            .await;
        let error = match rejected_update {
            Err(crate::SessionUpdateError::Rejected(error)) => error,
            other => panic!("U2 must be rejected: {other:?}"),
        };
        assert_eq!(error.code, "web_search_rejected", "U2/O0");
        typed.push(("U2", error.kind, error.code.clone()));
        assert_eq!(
            fixture.repository.get(&created.session_id).await.unwrap(),
            before,
            "U1/U2 O1 no root write"
        );
        assert_eq!(
            fixture.effects(),
            effects,
            "U1/U2 O1 no preparation or dispatch"
        );
        answers.push(typed);
    }
    assert_eq!(
        answers[0], answers[1],
        "NC rewording the message changes neither classification nor precedence"
    );
}
