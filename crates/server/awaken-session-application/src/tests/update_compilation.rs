//! Update compilation follows authored fields, never a second frozen-config admission.
use super::{
    FaultingSessionRepository, NoopMcpRealizer, RecordingEnvironmentSource, authority, create,
    persisted, realization,
};
use crate::{
    SessionApplication, SessionApplicationBuilder, SessionFieldUpdate, SessionMcpUpdate,
    SessionMetadataUpdate, SessionPersistence, SessionUpdateCommand, SessionUpdateCommands,
    SessionUpdateError,
};
use awaken_agent_contract::{McpTarget, ToolPolicyOverride, ToolsetPolicy, ToolsetSource};
use awaken_session_configuration_contract::SessionToolConfiguration;
use awaken_session_contract::{PersistedSession, SessionExecutionState};
use awaken_session_contract::{SessionRepository as _, SessionRepositoryReads as _};
use awaken_session_types::{SessionId, SessionRuntimePlacement};
use awaken_time::system_clock;
use awaken_tool_contract::{
    ToolConfigurationCompiler, ToolConfigurationError, ToolConfigurationInput,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Compiler {
    calls: AtomicUsize,
    reject: bool,
}

impl Compiler {
    fn new(reject: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            reject,
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ToolConfigurationCompiler for Compiler {
    fn compile(
        &self,
        input: ToolConfigurationInput<'_>,
    ) -> Result<serde_json::Value, ToolConfigurationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.reject {
            return Err(ToolConfigurationError {
                path: "policy".into(),
                code: "test_compilation_refused".into(),
                message: "explicitly injected compiler refusal".into(),
            });
        }
        assert_eq!(input.tool_name, "opaque");
        assert!(
            matches!(input.source, ToolsetSource::Mcp { server_name } if server_name == "frozen-source")
        );
        Ok(serde_json::json!({"normalized": input.configuration}))
    }
}

fn tools(value: &str) -> SessionToolConfiguration {
    SessionToolConfiguration {
        toolsets: vec![ToolsetPolicy {
            source: ToolsetSource::Mcp {
                server_name: "frozen-source".into(),
            },
            default: Default::default(),
            overrides: vec![ToolPolicyOverride::with_optional_configuration(
                "opaque",
                Default::default(),
                Some(serde_json::json!({"frozen": value})),
            )],
        }],
        client_tools: vec![],
    }
}

fn frozen(mut root: PersistedSession, managed: bool) -> PersistedSession {
    let configuration = tools("original");
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut root.baseline
    else {
        panic!("frozen fixture")
    };
    baseline.toolsets = configuration.toolsets.clone();
    if managed {
        baseline.mutation_policy = crate::session_resource::SessionMutationPolicy::Managed;
    }
    root.tools = configuration;
    root
}

fn application_with_compiler(
    repository: Arc<dyn crate::SessionStore>,
    runtime: Arc<realization::RecordingResourceRuntime>,
    compiler: Arc<Compiler>,
) -> SessionApplication {
    SessionApplicationBuilder::fixture(
        runtime,
        Arc::new(NoopMcpRealizer),
        SessionPersistence::new(repository, system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        compiler,
    )
    .build()
    .expect("complete test application")
}

fn command(key: &str) -> SessionUpdateCommand {
    SessionUpdateCommand {
        title: None,
        metadata: None,
        budget: None,
        tools: None,
        mcp_update: None,
        idempotency_key: Some(key.into()),
        request_fingerprint: format!("payload:{key}"),
        if_match: None,
    }
}

async fn repository() -> Arc<awaken_session_store::SqliteManagedSessionRepository> {
    Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    )
}

fn credential_root(id: &str) -> PersistedSession {
    use awaken_credential_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, ModelExposurePolicy,
    };
    let prepared = authority::unattempted_profiled_release::unattempted_with_placement(
        id,
        SessionRuntimePlacement::Worker,
    );
    // The old helper deliberately strips private ledgers for legacy tests.
    // Credential adoption needs a KNOWN empty materialization history. Use the
    // real fresh constructor instead of inventing that fact on a legacy DTO.
    let mut root = PersistedSession::frozen_with_budget(
        id,
        prepared.frozen_baseline().unwrap().clone(),
        prepared.resources,
        prepared.mcp,
        None,
        Default::default(),
        prepared.tools,
        prepared.budget,
    );
    root.execution = SessionExecutionState::Preparing;
    root.created_at_unix_ms = Some(1);
    let mut root = frozen(root, false);
    let holder = root
        .frozen_baseline()
        .unwrap()
        .environment
        .credential_realization
        .mcp_holder
        .clone();
    root.mcp = crate::session_mcp::SessionMcpAttachmentSet::from_initial(
        vec![crate::session_mcp::McpAttachmentDraft {
            name: "pending-mcp".into(),
            target: McpTarget::parse_http("https://example.test/mcp").unwrap(),
            prompts_as_skills: false,
            credential: Some(CredentialAccess::new(
                CredentialRef {
                    id: "source-a".into(),
                    revision: 3,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
            )),
            origin: crate::session_mcp::McpAttachmentOrigin::Session,
        }],
        Some(holder),
    )
    .unwrap();
    assert!(awaken_session_contract::session_can_adopt_mcp_credential_lifecycle(&root));
    root
}

fn revoke(key: &str) -> SessionUpdateCommand {
    SessionUpdateCommand {
        mcp_update: Some(SessionMcpUpdate::CredentialLifecycle {
            source_id: "source-a".into(),
            revoked: true,
            candidates: vec![],
        }),
        ..command(key)
    }
}

#[tokio::test]
async fn title_and_metadata_preserve_frozen_tools_without_compilation() {
    // UC1: nonempty opaque frozen settings + rejecting compiler. A successful
    // unrelated command must commit its own fields, preserve tools and emit no
    // tool projection. Empty toolsets would not exercise the former regression.
    let repository = repository().await;
    let id = SessionId::from("compile-unrelated");
    create(
        repository.as_ref(),
        frozen(persisted(id.as_str(), false, "idle"), false),
    )
    .await;
    let before = repository.get(&id).await.unwrap();
    let compiler = Compiler::new(true);
    let runtime = Arc::new(realization::RecordingResourceRuntime::default());
    let app = application_with_compiler(repository.clone(), runtime.clone(), compiler.clone());
    let updated = app
        .update_commands
        .update_session(
            &id,
            SessionUpdateCommand {
                title: Some(SessionFieldUpdate::Replace("new title".into())),
                metadata: Some(SessionMetadataUpdate::Patch(
                    std::collections::BTreeMap::from([("owner".into(), Some("user".into()))]),
                )),
                ..command("unrelated")
            },
        )
        .await
        .unwrap();
    assert!(updated.command_applied);
    assert!(updated.changes.title() && updated.changes.metadata() && !updated.changes.tools());
    assert_eq!(updated.session.title.as_deref(), Some("new title"));
    assert_eq!(
        updated.session.metadata.get("owner").map(String::as_str),
        Some("user")
    );
    assert_eq!(updated.session.tools, before.tools);
    assert_eq!(updated.session.baseline, before.baseline);
    assert_eq!(repository.get(&id).await.unwrap(), updated.session);
    assert_eq!(compiler.calls(), 0);
    assert!(runtime.replaced_tools.lock().unwrap().is_empty());
}

#[tokio::test]
async fn preparing_credential_revocation_preserves_tools_and_reserved_activity() {
    // UC2: exact server-owned revocation is admitted before first realization,
    // both with and without an already reserved activity. It changes the real
    // desired credential generation, never reauthors the Profile or tools.
    for reserved in [false, true] {
        let repository = repository().await;
        let id = SessionId::from(format!("compile-credential-{reserved}"));
        create(repository.as_ref(), credential_root(id.as_str())).await;
        let compiler = Compiler::new(true);
        let runtime = Arc::new(realization::RecordingResourceRuntime::default());
        let app = application_with_compiler(repository.clone(), runtime.clone(), compiler.clone());
        if reserved {
            app.activities.begin_activity(&id).await.unwrap();
        }
        let before = repository.get(&id).await.unwrap();
        assert!(awaken_session_contract::session_can_adopt_mcp_credential_lifecycle(&before));
        let updated = app
            .update_commands
            .update_session(&id, revoke("revoke"))
            .await
            .unwrap();
        assert!(updated.command_applied && updated.changes.mcp());
        assert_ne!(updated.session.mcp, before.mcp, "actual credential change");
        assert!(updated.session.mcp.desired_attachments().is_empty());
        assert_eq!(updated.session.tools, before.tools);
        assert_eq!(updated.session.baseline, before.baseline);
        assert_eq!(updated.session.resources, before.resources);
        assert_eq!(updated.session.environment, before.environment);
        assert_eq!(
            updated.session.environment_materialization(),
            before.environment_materialization()
        );
        assert_eq!(updated.session.realization, before.realization);
        assert_eq!(
            updated.session.active_activity_epochs,
            before.active_activity_epochs
        );
        assert_eq!(updated.session.run_activities(), before.run_activities());
        assert_eq!(repository.get(&id).await.unwrap(), updated.session);
        assert_eq!(compiler.calls(), 0);
        assert!(runtime.replaced_tools.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn explicit_tool_update_normalizes_before_one_commit_and_projects_committed_value() {
    // UC3: explicit opaque replacement compiles once in this CAS attempt;
    // Some(empty) really clears the old nonempty settings. Both values own the
    // durable truth and post-commit projection, with an exact If-Match.
    for clear in [false, true] {
        let repository = repository().await;
        let id = SessionId::from(format!("compile-explicit-{clear}"));
        create(
            repository.as_ref(),
            frozen(persisted(id.as_str(), false, "idle"), true),
        )
        .await;
        let compiler = Compiler::new(false);
        let runtime = Arc::new(realization::RecordingResourceRuntime::default());
        let app = application_with_compiler(repository.clone(), runtime.clone(), compiler.clone());
        let authored = if clear {
            Default::default()
        } else {
            tools("replacement")
        };
        let mut expected = authored.clone();
        if !clear {
            expected.toolsets[0].overrides[0].configuration =
                Some(serde_json::json!({"normalized":{"frozen":"replacement"}}));
        }
        let before = repository.get(&id).await.unwrap();
        let updated = app
            .update_commands
            .update_session(
                &id,
                SessionUpdateCommand {
                    tools: Some(authored),
                    if_match: Some(before.revision),
                    ..command("explicit")
                },
            )
            .await
            .unwrap();
        assert!(updated.command_applied && updated.changes.tools());
        assert_eq!(updated.command_revision.0, before.revision.0 + 1);
        assert_eq!(updated.session.tools, expected);
        assert_eq!(repository.get(&id).await.unwrap(), updated.session);
        assert_eq!(compiler.calls(), usize::from(!clear));
        assert_eq!(
            *runtime.replaced_tools.lock().unwrap(),
            vec![(id.to_string(), expected)]
        );
    }
}

#[tokio::test]
async fn rejected_explicit_compilation_preserves_root_receipt_and_effects() {
    // UC4: failure happens before the root transaction, command receipt or any
    // disposable projection; its classification and configuration path survive.
    let repository = repository().await;
    let id = SessionId::from("compile-reject");
    create(
        repository.as_ref(),
        frozen(persisted(id.as_str(), false, "idle"), true),
    )
    .await;
    let before = repository.get(&id).await.unwrap();
    let compiler = Compiler::new(true);
    let runtime = Arc::new(realization::RecordingResourceRuntime::default());
    let app = application_with_compiler(repository.clone(), runtime.clone(), compiler.clone());
    let request = SessionUpdateCommand {
        tools: Some(tools("invalid")),
        ..command("reject")
    };
    let key = format!(
        "managed:update-command:{id}:{}",
        SessionUpdateCommands::update_operation_id(id.as_str(), "reject")
    );
    let error = app
        .update_commands
        .update_session(&id, request)
        .await
        .unwrap_err();
    assert!(
        matches!(error, SessionUpdateError::Rejected(ref error) if error.code == "test_compilation_refused" && error.to_string().contains("toolsets[0].overrides[0].configuration.policy"))
    );
    assert_eq!(repository.get(&id).await.unwrap(), before);
    assert!(
        repository
            .idempotency_receipt(&id, &key)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(compiler.calls(), 1);
    assert!(runtime.replaced_tools.lock().unwrap().is_empty());
}

#[tokio::test]
async fn admission_precedence_still_rejects_stale_nonidle_and_mixed_commands_without_compilation() {
    // UC5: phase precedes If-Match; mixed credential/authoring fields precede
    // both. Missing legacy materialization history remains unknown. Omitting
    // tools cannot bypass any of these existing root admission fences.
    for rule in [
        "stale",
        "nonidle-stale",
        "mixed-credential",
        "unknown-history",
    ] {
        let repository = repository().await;
        let id = SessionId::from(format!("compile-precedence-{rule}"));
        let phase = if rule == "stale" { "idle" } else { "running" };
        let root = if rule == "unknown-history" {
            let mut wire = serde_json::to_value(credential_root(id.as_str())).unwrap();
            wire.as_object_mut()
                .unwrap()
                .remove("environment_materialization");
            let legacy: PersistedSession = serde_json::from_value(wire).unwrap();
            assert!(legacy.environment_materialization().is_none());
            assert!(!awaken_session_contract::session_can_adopt_mcp_credential_lifecycle(&legacy));
            legacy
        } else {
            frozen(persisted(id.as_str(), false, phase), true)
        };
        create(repository.as_ref(), root).await;
        let before = repository.get(&id).await.unwrap();
        let compiler = Compiler::new(true);
        let runtime = Arc::new(realization::RecordingResourceRuntime::default());
        let app = application_with_compiler(repository.clone(), runtime.clone(), compiler.clone());
        let mut request = command(rule);
        request.title = Some(SessionFieldUpdate::Replace("rejected".into()));
        request.if_match = Some(awaken_session_types::SessionRevision(before.revision.0 + 1));
        if rule == "mixed-credential" {
            request.mcp_update = revoke(rule).mcp_update;
        }
        if rule == "unknown-history" {
            request.title = None;
            request.mcp_update = revoke(rule).mcp_update;
            request.if_match = Some(before.revision);
        }
        let error = app
            .update_commands
            .update_session(&id, request)
            .await
            .unwrap_err();
        match rule {
            "stale" => assert!(matches!(error, SessionUpdateError::Conflict)),
            "nonidle-stale" | "unknown-history" => {
                assert!(matches!(error, SessionUpdateError::NotIdle))
            }
            _ => assert!(
                matches!(error, SessionUpdateError::Rejected(ref error) if error.to_string().contains("credential lifecycle cannot mutate Session authoring fields"))
            ),
        }
        assert_eq!(repository.get(&id).await.unwrap(), before);
        assert_eq!(compiler.calls(), 0);
        assert!(runtime.replaced_tools.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn exact_replay_after_reopen_and_later_write_never_recompiles_original_payload() {
    // UC6: title, credential and explicit-tool receipts survive a file-backed
    // SQLite reopen plus a later independent root. A replacement rejecting
    // compiler cannot reject a previously committed command or advance it.
    for kind in ["title", "credential", "tools"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.db");
        let repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(path.to_str().unwrap())
                .await
                .unwrap(),
        );
        let id = SessionId::from(format!("compile-reopen-{kind}"));
        create(
            repository.as_ref(),
            if kind == "credential" {
                credential_root(id.as_str())
            } else {
                frozen(persisted(id.as_str(), false, "idle"), true)
            },
        )
        .await;
        let compiler = Compiler::new(false);
        let runtime = Arc::new(realization::RecordingResourceRuntime::default());
        let app = application_with_compiler(repository.clone(), runtime.clone(), compiler.clone());
        let mut request = if kind == "credential" {
            revoke(kind)
        } else {
            command(kind)
        };
        if kind == "title" {
            request.title = Some(SessionFieldUpdate::Replace("original command".into()));
        }
        if kind == "tools" {
            request.tools = Some(tools("replacement"));
        }
        let first = app
            .update_commands
            .update_session(&id, request.clone())
            .await
            .unwrap();
        assert!(first.command_applied);
        assert_eq!(compiler.calls(), usize::from(kind == "tools"));
        // A separate actual root command, independent of the tested receipt.
        if kind == "credential" {
            app.activities.begin_activity(&id).await.unwrap();
        } else {
            app.update_commands
                .update_session(
                    &id,
                    SessionUpdateCommand {
                        metadata: Some(SessionMetadataUpdate::Patch(
                            std::collections::BTreeMap::from([(
                                "later".into(),
                                Some("preserved".into()),
                            )]),
                        )),
                        ..command("later-root")
                    },
                )
                .await
                .unwrap();
        }
        let current = repository.get(&id).await.unwrap();
        assert!(current.revision > first.command_revision);
        drop(app);
        drop(repository);
        let reopened = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(path.to_str().unwrap())
                .await
                .unwrap(),
        );
        let refusing = Compiler::new(true);
        let rebuilt = application_with_compiler(reopened.clone(), runtime, refusing.clone());
        let replay = rebuilt
            .update_commands
            .update_session(&id, request)
            .await
            .unwrap();
        assert!(!replay.command_applied && !replay.changes.any());
        assert_eq!(replay.command_revision, first.command_revision);
        assert_eq!(replay.session, current);
        assert_eq!(reopened.get(&id).await.unwrap(), current);
        assert_eq!(refusing.calls(), 0);
    }
}

#[tokio::test]
async fn receipt_committed_after_initial_miss_precedes_compilation_of_delayed_update() {
    // UC6 race: stop the delayed call at its first real root read, after the
    // public receipt lookup returned None. A different application commits the
    // exact request, then a later root, before that read is released. The inner
    // receipt recheck must win over the delayed application's rejecting compiler.
    let durable = repository().await;
    let id = SessionId::from("compile-receipt-race");
    create(
        durable.as_ref(),
        frozen(persisted(id.as_str(), false, "idle"), true),
    )
    .await;
    let gated = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    gated
        .paused_gets
        .lock()
        .unwrap()
        .insert(id.to_string(), gate.clone());
    let runtime = Arc::new(realization::RecordingResourceRuntime::default());
    let refusing = Compiler::new(true);
    let delayed_app = Arc::new(application_with_compiler(
        gated.clone(),
        runtime.clone(),
        refusing.clone(),
    ));
    let request = SessionUpdateCommand {
        tools: Some(tools("replacement")),
        ..command("race")
    };
    let task = tokio::spawn({
        let id = id.clone();
        let request = request.clone();
        async move {
            delayed_app
                .update_commands
                .update_session(&id, request)
                .await
        }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        gated.get_entered.notified(),
    )
    .await
    .unwrap();
    assert!(!task.is_finished());
    let accepting = Compiler::new(false);
    let winner_app = application_with_compiler(durable.clone(), runtime, accepting.clone());
    let winner = winner_app
        .update_commands
        .update_session(&id, request)
        .await
        .unwrap();
    let later = winner_app
        .update_commands
        .update_session(
            &id,
            SessionUpdateCommand {
                title: Some(SessionFieldUpdate::Replace("later root".into())),
                ..command("later")
            },
        )
        .await
        .unwrap();
    assert!(later.command_revision > winner.command_revision);
    gated.paused_gets.lock().unwrap().remove(id.as_str());
    gate.add_permits(1);
    let replay = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!replay.command_applied && !replay.changes.any());
    assert_eq!(replay.command_revision, winner.command_revision);
    assert_eq!(replay.session, later.session);
    assert_eq!(durable.get(&id).await.unwrap(), later.session);
    assert_eq!(accepting.calls(), 1);
    assert_eq!(refusing.calls(), 0);
}
