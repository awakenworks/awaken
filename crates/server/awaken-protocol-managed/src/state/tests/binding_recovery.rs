use super::*;

struct RevisionedProfiles(std::sync::atomic::AtomicU64);

impl RevisionedProfiles {
    fn profile(
        agent_id: &str,
        revision: u64,
    ) -> awaken_executable_agent_contract::ExecutableAgentSessionProfile {
        let delegates = if agent_id == "coordinator" {
            vec![awaken_executable_agent_contract::ExecutableAgentDelegate {
                agent_id: "researcher".into(),
                source_revision: Some(revision),
            }]
        } else {
            Vec::new()
        };
        awaken_executable_agent_contract::ExecutableAgentSessionProfile {
            name: Some(format!("{agent_id}-v{revision}")),
            description: Some(format!("revision {revision}")),
            source_revision: revision,
            model: Some("test-model".into()),
            execution_model_ref: Some("test-model".into()),
            backend_ref: "native".into(),
            delegates,
            ..Default::default()
        }
    }
}

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for RevisionedProfiles {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        Some(Self::profile(
            agent_id,
            self.0.load(std::sync::atomic::Ordering::SeqCst),
        ))
    }

    fn session_profile_at_revision_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        Some(Self::profile(agent_id, source_revision))
    }
}

#[tokio::test]
async fn cold_projection_uses_the_session_pinned_agent_revision() {
    // Cause/effect graph: C1 Session freezes coordinator revision 7; C2 the
    // current catalog advances to revision 8; C3 the disposable Managed cache is
    // lost; C4 durable recovery reads the baseline pin. Effects: E1 root identity
    // remains v7; E2 the full child definition remains v7; E3 no current-v8
    // presentation leaks into the historical Session.
    //
    // Decision table: R1 C1+!C2 -> E1+E2; R2 C1+C2+!C3 -> cached E1+E2;
    // R3 C1+C2+C3+C4 -> rebuilt E1+E2+E3. FMECA: resolving current state during
    // R3 would silently rewrite an old Session's Agent name, version, model, and
    // child roster after restart (high severity, externally visible); the durable
    // `agent_revision` pin and exact profile lookup detect and eliminate it.
    let repo = Arc::new(ephemeral_session_repo());
    let profiles = Arc::new(RevisionedProfiles(std::sync::atomic::AtomicU64::new(7)));
    let state = ManagedState::new_with_mcp(RehydrateFake::default())
        .with_config_source(profiles.clone())
        .with_session_repo(repo);
    let request = serde_json::from_value(serde_json::json!({
        "agent":"coordinator", "environment_id":"env_local"
    }))
    .unwrap();
    let created = state.create_session(request, None).await.unwrap();
    assert_eq!(created.agent.name, "coordinator-v7", "R1");
    profiles.0.store(8, std::sync::atomic::Ordering::SeqCst);
    state.sessions.lock().unwrap().remove(&created.id);
    state.ensure_session(&created.id).await.unwrap();
    let recovered = state.get_session(&created.id).unwrap();
    assert_eq!(recovered.agent.name, "coordinator-v7", "R3/E1");
    assert_eq!(recovered.agent.version, 7, "R3/E1");
    let child = &recovered
        .agent
        .multiagent
        .as_ref()
        .expect("coordinator roster")
        .agents[0]
        .as_agent()
        .expect("first roster entry is a child agent");
    assert_eq!(child.name, "researcher-v7", "R3/E2-E3");
    assert_eq!(child.version, 7, "R3/E2-E3");
}

#[tokio::test]
async fn immediate_environment_binding_sink_is_durable_and_idempotent() {
    // Cause/effect table: C1 durable Session exists, C2 binding absent,
    // C3 identical binding already committed. R1 C1+C2 -> persist binding
    // and advance revision; R2 C1+C3 -> success without another revision.
    use awaken_session_contract::SessionEnvironmentBindingSink;

    let repo = Arc::new(ephemeral_session_repo());
    create_session_fixture(
        repo.as_ref(),
        DEFAULT_SCOPE,
        sample_persisted("binding-now"),
    )
    .await;
    let sink = awaken_session_application::RepositoryEnvironmentBindingSink::new(repo.clone());

    let receipt = awaken_session_contract::SessionEnvironmentReceipt::new(
        "binding-now",
        awaken_session_contract::SessionEnvironmentEffectKind::Create,
        "opaque-handle",
        None,
    );
    sink.persist(receipt.clone()).await.unwrap();
    let first = repo.get("binding-now").await.unwrap();
    assert_eq!(first.environment.binding(), Some("opaque-handle"));
    sink.persist(receipt).await.unwrap();
    let replay = repo.get("binding-now").await.unwrap();
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.environment, first.environment);
}

struct ConflictInjectingRepo {
    inner: Arc<SqliteManagedSessionRepository>,
    conflicts: AtomicU64,
}

#[async_trait]
impl ManagedSessionRepository for ConflictInjectingRepo {
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: awaken_session_contract::IdempotencyRecord,
        facts: Vec<awaken_session_contract::ManagedLifecycleFact>,
    ) -> Result<
        awaken_session_contract::SessionRevision,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner
            .create(owner_scope, session, idempotency, facts)
            .await
    }

    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: awaken_session_contract::SessionMutation,
    ) -> Result<
        awaken_session_contract::SessionMutationResult,
        awaken_session_contract::SessionRepositoryError,
    > {
        if self
            .conflicts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            return Ok(awaken_session_contract::SessionMutationResult::Conflict {
                current_revision: mutation.expected_revision,
            });
        }
        self.inner.commit_mutation(owner_scope, mutation).await
    }

    async fn append_lifecycle(
        &self,
        fact: awaken_session_contract::ManagedLifecycleFact,
    ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
        self.inner.append_lifecycle(fact).await
    }

    async fn pending_lifecycle(
        &self,
    ) -> Result<
        Vec<awaken_session_contract::ManagedLifecycleFact>,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner.pending_lifecycle().await
    }

    async fn complete_lifecycle(
        &self,
        fact_id: &str,
    ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
        self.inner.complete_lifecycle(fact_id).await
    }

    async fn get(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, awaken_session_contract::SessionRepositoryError> {
        self.inner.get(session_id).await
    }

    async fn reconcilable_sessions(
        &self,
    ) -> Result<
        awaken_session_contract::SessionRecoveryScan,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner.reconcilable_sessions().await
    }

    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<
        Option<awaken_session_contract::SessionIdempotencyReceipt>,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner.idempotency_receipt(session_id, key).await
    }

    async fn owner(
        &self,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::SessionRepositoryError> {
        self.inner.owner(session_id).await
    }
}

#[tokio::test]
async fn immediate_binding_cas_retries_once_then_fails_closed_at_the_bound() {
    // Cause/effect table: C1 conflict count below the three-attempt bound,
    // C2 conflict count reaches the bound. R1 C1 -> retry then persist;
    // R2 C2 -> error and leave the binding absent. These are the two
    // equivalence classes for the moved application-owned CAS loop.
    use awaken_session_contract::SessionEnvironmentBindingSink;

    for (case, conflicts, accepted) in [
        ("one conflict then success", 1, true),
        ("three conflicts exhaust bound", 3, false),
    ] {
        let inner = Arc::new(ephemeral_session_repo());
        create_session_fixture(
            inner.as_ref(),
            DEFAULT_SCOPE,
            sample_persisted(&format!("binding-{conflicts}")),
        )
        .await;
        let repo = Arc::new(ConflictInjectingRepo {
            inner: inner.clone(),
            conflicts: AtomicU64::new(conflicts),
        });
        let sink = awaken_session_application::RepositoryEnvironmentBindingSink::new(repo);
        let receipt = awaken_session_contract::SessionEnvironmentReceipt::new(
            format!("binding-{conflicts}"),
            awaken_session_contract::SessionEnvironmentEffectKind::Create,
            "opaque",
            None,
        );
        let result = sink.persist(receipt).await;
        assert_eq!(result.is_ok(), accepted, "{case}");
        assert_eq!(
            inner
                .get(&format!("binding-{conflicts}"))
                .await
                .unwrap()
                .environment
                .binding(),
            accepted.then_some("opaque"),
            "{case}"
        );
    }
}

pub(in crate::state) fn sample_inputs() -> awaken_session_contract::ResolvedSessionResources {
    awaken_session_contract::ResolvedSessionResources {
        inputs: vec![awaken_session_contract::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from("input-file"),
            source: awaken_session_contract::ResolvedInputSource::File {
                file_id: awaken_resource_contract::FileId::from("file-hash"),
            },
            mount_path: "/input.txt".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }],
        skills: None,
    }
}

#[derive(Clone, Copy)]
enum ApplicationCasRule {
    Apply,
    Replay,
    Stale,
}

#[tokio::test]
async fn application_root_cas_decision_table() {
    // Cause-effect graph:
    // C1 payload/key already committed -> E1 replay the committed revision.
    // !C1 + C2 expected root revision is current -> E2 apply once.
    // !C1 + !C2 -> E3 conflict; the stale snapshot is never merged.
    //
    // | Rule | C1 same receipt | C2 current revision | Effect |
    // |------|-----------------|---------------------|--------|
    // | A1   | F               | T                   | apply  |
    // | A2   | T               | -                   | replay |
    // | A3   | F               | F                   | 409    |
    //
    // The rows generate the cases below against the real SQLite adapter and
    // the one application command compiler, not a duplicate fake algorithm.
    for (index, rule) in [
        ApplicationCasRule::Apply,
        ApplicationCasRule::Replay,
        ApplicationCasRule::Stale,
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("sesn_application_cas_{index}");
        let repo = Arc::new(ephemeral_session_repo());
        create_session_fixture(repo.as_ref(), "workspace-a", sample_persisted(&id)).await;
        let stale = repo.get(&id).await.unwrap();
        let state =
            ManagedState::new_with_mcp(RehydrateFake::default()).with_session_repo(repo.clone());
        let applied = state
            .commit_session_snapshot(
                "workspace-a",
                stale.clone(),
                "decision-table-first",
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            applied.revision,
            awaken_session_contract::SessionRevision(2)
        );

        match rule {
            ApplicationCasRule::Apply => {}
            ApplicationCasRule::Replay => {
                let replayed = state
                    .commit_session_snapshot(
                        "workspace-a",
                        stale,
                        "decision-table-first",
                        Vec::new(),
                    )
                    .await
                    .unwrap();
                assert_eq!(replayed.revision, applied.revision);
            }
            ApplicationCasRule::Stale => {
                assert!(matches!(
                    state
                        .commit_session_snapshot(
                            "workspace-a",
                            stale,
                            "decision-table-stale",
                            Vec::new(),
                        )
                        .await,
                    Err(StateError::Conflict)
                ));
                assert_eq!(repo.get(&id).await.unwrap().revision, applied.revision);
            }
        }
    }
}

#[test]
fn rehydrated_session_restores_persisted_config() {
    // Cause graph: a durable mutable tool set is the exact replacement;
    // only a genuinely non-durable in-memory Session derives Runtime defaults.
    //
    // | Rule | Persisted tools | Projection |
    // |---|---|---|
    // | T1 | durable row, including empty | exact durable value |
    // | T2 | no durable row | Runtime default for transient projection |
    let state = ManagedState::new_with_mcp(RehydrateFake::default());
    let mut persisted = sample_persisted("sesn_1");
    persisted.tools =
        crate::project::session_tool_configuration(&[awaken_session_contract::AgentTool::Custom {
            name: "durable-tool".into(),
            description: "Client-executed tool".into(),
            input_schema: awaken_session_contract::CustomToolInputSchema::from_value(
                serde_json::json!({"type": "object"}),
            )
            .unwrap(),
        }]);
    let session = state
        .rehydrated_session("sesn_1", DEFAULT_SCOPE, Some(persisted))
        .expect("valid durable projection");
    assert_eq!(session.agent.id, "coder");
    assert_eq!(session.agent.model.id, "kimi-k2");
    assert_eq!(session.title.as_deref(), Some("My session"));
    assert_eq!(
        session.metadata.get("team").map(String::as_str),
        Some("research")
    );
    assert_eq!(
        session.agent.mcp_servers.len(),
        1,
        "the accepted MCP server is restored"
    );
    assert!(matches!(
        &session.agent.tools[..],
        [awaken_session_contract::AgentTool::Custom { name, .. }] if name == "durable-tool"
    ));
    assert!(
        session.resources.is_empty(),
        "the stored Session DTO must not duplicate typed resource state"
    );
}

#[test]
fn rehydrated_session_falls_back_without_persisted_config() {
    let state = ManagedState::new_with_mcp(RehydrateFake::default());
    let session = state
        .rehydrated_session("sesn_1", DEFAULT_SCOPE, None)
        .expect("legacy fallback projection");
    assert_eq!(session.agent.id, "assistant");
    assert_eq!(session.agent.model.id, "host-default-model");
    assert!(session.title.is_none());
    assert!(session.agent.mcp_servers.is_empty());
}

#[test]
fn persisted_session_rejects_corrupt_tools_before_rehydration() {
    // Causal graph:
    // durable tool projection is present but invalid
    //   -> typed store decoding fails
    //   -> Runtime defaults are not substituted
    //   -> caller cannot cache or expose a weaker Session.
    //
    // Decision table:
    // | Durable field | Shape | Expected behavior |
    // | absent | n/a | decode the explicit empty default for legacy rows |
    // | present | valid typed tool | restore exact tool |
    // | present | invalid | decoding error; no fallback |
    let persisted = sample_persisted("sesn_corrupt");
    let mut value = serde_json::to_value(persisted).unwrap();
    value["tools"] = serde_json::json!({"unexpected": true});

    assert!(
        serde_json::from_value::<PersistedSession>(value).is_err(),
        "corrupt durable capabilities fail at the store decoding boundary and cannot reach rehydration"
    );
}

#[tokio::test]
async fn ensure_session_rehydrates_from_repo_after_cache_loss() {
    // Cause/effect graph: C1 cache is cold; C2 durable Session and committed
    // transcript exist; C3 Runtime projection is stale; C4 a durable child Run
    // has its own committed transcript. Effects: E1 rebuild the HTTP/history/
    // delegation read model; E2 perform no Environment, Resource, or MCP
    // realization; E3 expose the child transcript only through its child Thread.
    // Decision rules: R1 C1+C2+C3+!C4 => E1+E2; R2 C1+C2+C3+C4 => E1+E2+E3.
    // Runtime recovery belongs to the explicit reconciler/run-admission tests
    // below. FMECA:
    // driving effects from GET can duplicate mounts/credentials or turn an
    // outage into a read failure (S8/O5/D7), so cold reads remain pure; flattening
    // child tool events into the primary Thread destroys context isolation
    // (S8/O4/D6), so R2 asserts both inclusion and exclusion projections.
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let mut persisted = sample_persisted("sesn_1");
    persisted.environment.set_resident("opaque-runtime-binding");
    persisted.realization = Some(awaken_session_contract::SessionRealizationLease {
        owner: "managed-runtime/prior-boot".into(),
        runtime_incarnation: "managed-runtime/prior-boot".into(),
        epoch: 3,
        expires_at_unix_ms: 0,
    });
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, persisted).await;

    // Fresh state (empty cache) sharing the durable repo — simulates a restart.
    let runtime = RehydrateFake::default();
    *runtime.committed.lock().unwrap() = Some(vec![Message::new(
        awaken_agent_contract::agent::message::Id("assistant-tool".into()),
        awaken_agent_contract::agent::message::Role::Assistant,
        vec![
            awaken_agent_contract::agent::content::ContentBlock::ToolUse {
                id: "call-submit".into(),
                name: "design_submit_artifact".into(),
                input: serde_json::json!({"manifest_path":"artifact-manifest.json"}),
            },
        ],
    )]);
    *runtime.pending.lock().unwrap() = Some(awaken_session_contract::Pending {
        tool_use_id: "call-submit".into(),
        name: "design_submit_artifact".into(),
        input: serde_json::json!({"manifest_path":"artifact-manifest.json"}),
        client_executed: true,
    });
    runtime.committed_by_thread.lock().unwrap().insert(
        "child-durable".into(),
        vec![Message::new(
            awaken_agent_contract::agent::message::Id("child-assistant-tool".into()),
            awaken_agent_contract::agent::message::Role::Assistant,
            vec![
                awaken_agent_contract::agent::content::ContentBlock::ToolUse {
                    id: "child-tool-use-1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({"query":"managed child isolation"}),
                },
            ],
        )],
    );
    runtime.delegated.lock().unwrap().push(DelegatedRun {
        run_id: awaken_agent_contract::agent::run::Id("child-durable".into()),
        parent_call_id: "call-durable".into(),
        agent_id: "researcher".into(),
        status: awaken_agent_contract::agent::delegation::DelegationStatus::Completed,
    });
    let restored = runtime.restored.clone();
    let restored_environments = runtime.restored_environments.clone();
    let restored_runtimes = runtime.restored_runtimes.clone();
    let order = runtime.order.clone();
    let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo.clone());
    restarted.ensure_session("sesn_1").await.expect("rehydrate");
    let session = restarted
        .get_session("sesn_1")
        .expect("session present after rehydrate");
    assert_eq!(
        session.agent.id, "coder",
        "real agent id, not the placeholder"
    );
    assert_eq!(session.title.as_deref(), Some("My session"));
    assert_eq!(session.agent.mcp_servers.len(), 1);
    assert_eq!(session.resources.len(), 1);
    assert!(matches!(
        &session.resources[0],
        crate::types::resource::SessionResource::File { file_id, .. }
            if file_id == "file-hash"
    ));
    let threads = restarted.list_threads("sesn_1").expect("threads restored");
    assert_eq!(threads.len(), 2, "primary plus the durable runtime child");
    assert!(threads.iter().any(|thread| {
        thread.id == "child-durable"
            && thread.agent.id == "researcher"
            && thread.status == SessionThreadStatus::Idle
    }));
    assert!(
        restored.lock().unwrap().is_empty(),
        "a read rebuilds no Runtime Resource projection"
    );
    assert_eq!(
        order.lock().unwrap().as_slice(),
        &["history", "delegations", "history"],
        "a read opens only the primary and child transcript/delegation projections"
    );
    assert!(restored_runtimes.lock().unwrap().is_empty(), "read-only");
    assert!(
        restored_environments.lock().unwrap().is_empty(),
        "read-only"
    );
    let durable = repo.get("sesn_1").await.unwrap();
    assert_eq!(
        durable.resources.active,
        sample_inputs(),
        "read preserves truth"
    );
    assert!(
        durable.resources.activations.is_empty(),
        "read does not adopt a legacy activation"
    );
    let events = restarted
        .list_events("sesn_1", None, None, false)
        .expect("list rehydrated events");
    let encoded = serde_json::to_value(events).expect("events serialize");
    assert!(
        encoded["data"].as_array().unwrap().iter().any(|event| {
            event["type"] == "agent.custom_tool_use" && event["id"] == "call-submit"
        })
    );
    let child_events = restarted
        .list_thread_events("sesn_1", "child-durable", None, None)
        .expect("list isolated child events");
    assert!(
        child_events.data.iter().any(|event| {
            event.id == "child-tool-use-1"
                && matches!(event.kind, OutboundKind::AgentToolUse { .. })
        }),
        "R2/E3"
    );
    let primary_events = restarted
        .list_thread_events("sesn_1", "sesn_1:primary", None, None)
        .expect("list isolated primary events");
    assert_eq!(
        primary_events
            .data
            .iter()
            .filter(|event| event.id == "call-submit")
            .count(),
        1,
        "R2/E3 child projection is not flattened into primary"
    );
}

#[tokio::test]
async fn committed_event_refresh_merges_peer_messages_exactly_once() {
    // Cause/effect graph:
    // C1 a durable Session is already cached on Coordinator A;
    // C2 Coordinator B commits a new Runtime message to the shared transcript;
    // C3 A refreshes once or repeatedly through the public-read seam.
    // E1 A exposes both committed messages; E2 each message is projected once;
    // E3 the in-memory cache never becomes an alternative source of truth.
    //
    // Decision table:
    // | cache | transcript delta | refresh count | result                 |
    // | warm  | none             | one          | unchanged              |
    // | warm  | one peer message | one          | append peer projection |
    // | warm  | same peer message| repeated     | no duplicate           |
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, sample_persisted("sesn_peer")).await;
    let runtime = RehydrateFake::default();
    let first = Message::text(
        awaken_agent_contract::agent::message::Id("peer-first".into()),
        awaken_agent_contract::agent::message::Role::Assistant,
        "first",
    );
    *runtime.committed.lock().unwrap() = Some(vec![first.clone()]);
    let state = ManagedState::new_with_mcp(runtime.clone()).with_session_repo(repo);
    state.ensure_session("sesn_peer").await.expect("warm cache");

    let second = Message::text(
        awaken_agent_contract::agent::message::Id("peer-second".into()),
        awaken_agent_contract::agent::message::Role::Assistant,
        "second",
    );
    *runtime.committed.lock().unwrap() = Some(vec![first, second]);
    state
        .refresh_committed_events("sesn_peer")
        .await
        .expect("merge peer commit");
    state
        .refresh_committed_events("sesn_peer")
        .await
        .expect("idempotent refresh");

    let events = state
        .list_events("sesn_peer", None, None, false)
        .expect("read refreshed projection")
        .data;
    let rendered = serde_json::to_string(&events).unwrap();
    assert_eq!(rendered.matches("first").count(), 1, "E1/E2");
    assert_eq!(rendered.matches("second").count(), 1, "E1/E2");
}

#[tokio::test]
async fn protocol_defaults_preparer_rehydrates_the_exact_durable_baseline() {
    // Phase-4 rule M5: an existing durable Session after process restart is
    // not merely "present".  The shared preparer must traverse the same
    // recovery path that reinstalls its frozen Resource and Environment
    // snapshot before a wire adapter may execute.
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let mut persisted = sample_persisted("external-thread");
    persisted.environment.set_resident("opaque-runtime-binding");
    create_session_fixture(repo.as_ref(), "workspace-a", persisted).await;

    let runtime = RehydrateFake::default();
    let restored = runtime.restored.clone();
    let restored_environments = runtime.restored_environments.clone();
    let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo);

    restarted
        .session_application()
        .admit_run_session("workspace-a", "external-thread", "ignored-on-recovery")
        .await
        .expect("prepare existing protocol thread");

    assert_eq!(restored.lock().unwrap().len(), 1);
    assert_eq!(restored.lock().unwrap()[0].2, sample_inputs());
    assert_eq!(
        restored_environments.lock().unwrap().as_slice(),
        &[(
            "coder".to_string(),
            "external-thread".to_string(),
            "opaque-runtime-binding".to_string(),
        )],
        "the durable agent and opaque Environment binding win over request-time defaults"
    );
}

#[tokio::test]
async fn session_id_mint_namespaces_restart_away_from_repository_truth() {
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, sample_persisted("sesn_0")).await;
    let restarted = ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo);
    let created = restarted
        .create_session(bare_create_params(), None)
        .await
        .expect("mint after restart");
    assert_ne!(created.id, "sesn_0");
    assert!(created.id.starts_with("sesn_fnv1a64:"));
}

#[tokio::test]
async fn active_active_session_id_mint_is_collision_free() {
    // Cause/effect graph: each Coordinator owns a distinct process
    // incarnation but both start their local sequence at zero and share one
    // Session repository.
    //
    // | Rule | incarnations | local sequence | shared repo | Effect |
    // |---|---|---|---|---|
    // | S1 | different | both zero | yes | two distinct committed Sessions |
    // | S2 | same process object | increasing | yes | distinct Sessions |
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let left = ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo.clone());
    let right = ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo.clone());
    let (left_result, right_result) = tokio::join!(
        left.create_session(bare_create_params(), None),
        right.create_session(bare_create_params(), None),
    );
    let left_session = left_result.expect("S1 left Session");
    let right_session = right_result.expect("S1 right Session");
    assert_ne!(left_session.id, right_session.id, "S1");
    assert!(repo.get(&left_session.id).await.is_ok(), "S1");
    assert!(repo.get(&right_session.id).await.is_ok(), "S1");

    let next = left
        .create_session(bare_create_params(), None)
        .await
        .expect("S2 next Session");
    assert_ne!(next.id, left_session.id, "S2");
    assert_ne!(next.id, right_session.id, "S2");
}

#[tokio::test]
async fn resource_reconciler_retries_and_commits_a_crash_interrupted_activation() {
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let mut pending = sample_persisted("sesn_pending");
    let desired = pending.resources.active.clone();
    pending.resources = Default::default();
    pending
        .resources
        .prepare("sesn_pending", desired.clone())
        .unwrap();
    pending.resources.start_attempt().unwrap();
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, pending).await;

    let runtime = RehydrateFake::default();
    let restored = runtime.restored.clone();
    let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo.clone());
    assert_eq!(restarted.reconcile_resource_activations().await, 1);
    restarted.ensure_session("sesn_pending").await.unwrap();

    assert_eq!(restored.lock().unwrap().len(), 1);
    assert_eq!(restored.lock().unwrap()[0].2, desired);
    let durable = repo.get("sesn_pending").await.unwrap();
    assert!(durable.resources.pending.is_none());
    assert_eq!(durable.resources.active, desired);
    assert_eq!(durable.resources.activations[0].attempts, 2);
    assert_eq!(
        durable.resources.activations[0].state,
        awaken_session_contract::ActivationState::Active
    );
}

#[tokio::test]
async fn resource_reclaimer_finishes_terminal_release_after_restart() {
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let mut deleted = sample_persisted("sesn_deleted");
    deleted.disposition = SessionDisposition::Deleting;
    deleted.execution = awaken_session_contract::SessionExecutionState::Terminated;
    deleted.resources.adopt_legacy_active("sesn_deleted");
    deleted.resources.begin_release().unwrap();
    create_session_fixture(repo.as_ref(), "workspace-a", deleted).await;

    let restarted =
        ManagedState::new_with_mcp(RehydrateFake::default()).with_session_repo(repo.clone());
    let report = restarted.application.reconcile_resource_activations().await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.settled.len(), 1);
    assert!(
        matches!(
            repo.get("sesn_deleted").await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "D3: a successful retry converges the hidden cleanup row to a tombstone"
    );
    assert!(
        repo.reconcilable_sessions()
            .await
            .unwrap()
            .sessions
            .is_empty()
    );
}

#[tokio::test]
async fn resource_reclaimer_never_tears_down_a_live_session_environment() {
    // Resource-reclaimer cause/effect graph:
    // C1 lifecycle is live (idle/running/rescheduling) or terminal; C2 an
    // active Resource generation exists; C3 another convergence concern keeps
    // the row in the broad repository scan. E1 live rows receive no Resource
    // apply/release/end effect; E2 terminal+active releases exactly once; E3 a
    // deleted row completes cleanup and tombstones. This reproduces the
    // production failure where C1=running+C2+C3 previously deleted a K8s Pod.
    //
    // | Rule | status | active | broad scan | apply | end | durable outcome |
    // |---|---|---|---|---|---|---|
    // | R1 | idle | true | environment | 0 | 0 | unchanged |
    // | R2 | running | true | environment | 0 | 0 | unchanged |
    // | R3 | rescheduling | true | environment | 0 | 0 | unchanged |
    // | R4 | terminated | true | terminal | 0 | 1 | released |
    // | R5 | deleted | false | deleted | 0 | 1 | tombstoned |
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    for status in ["idle", "running", "rescheduling", "terminated"] {
        let id = format!("sesn_{status}");
        let mut session = sample_persisted(&id);
        session.execution = status.parse().expect("fixture execution state");
        session.resources.adopt_legacy_active(&id);
        session
            .environment
            .set_resident(format!("binding-{status}"));
        create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, session).await;
    }
    let mut deleted = sample_persisted("sesn_deleted_empty");
    deleted.disposition = SessionDisposition::Deleting;
    deleted.execution = awaken_session_contract::SessionExecutionState::Terminated;
    deleted.resources = Default::default();
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, deleted).await;

    let runtime = RehydrateFake::default();
    let applied = runtime.restored.clone();
    let ended = runtime.ended.clone();
    let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo.clone());

    let report = restarted.application.reconcile_resource_activations().await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.settled.len(), 2);
    assert!(applied.lock().unwrap().is_empty(), "R1-R5");
    assert_eq!(
        ended.lock().unwrap().as_slice(),
        &["sesn_deleted_empty", "sesn_terminated"],
        "R4/R5; repository order is stable by Session id"
    );
    for status in ["idle", "running", "rescheduling"] {
        let session = repo.get(&format!("sesn_{status}")).await.unwrap();
        assert!(session.resources.has_active(), "R1-R3 {status}");
        assert_eq!(
            session.environment.binding(),
            Some(format!("binding-{status}").as_str())
        );
    }
    let terminated = repo.get("sesn_terminated").await.unwrap();
    assert!(!terminated.resources.has_active(), "R4");
    assert!(
        matches!(
            repo.get("sesn_deleted_empty").await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "R5"
    );
}

#[tokio::test]
async fn cold_rehydrate_preserves_a_running_sessions_resource_generation() {
    // Cold-rehydrate cause/effect rule C1: a broad durable read finds a
    // running Session with active Resources and a resident Environment after
    // process cache loss. E1 reinstalls only the readable projection; E2
    // performs no Resource, Environment, or terminal teardown effect.
    // The terminal and pending branches are covered by the reclaimer table
    // above and `ensure_session_retries_and_commits_a_crash_interrupted_activation`.
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let mut running = sample_persisted("sesn_running_rehydrate");
    running.execution = SessionExecutionState::Running;
    running
        .resources
        .adopt_legacy_active("sesn_running_rehydrate");
    running.environment.set_resident("running-binding");
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, running).await;

    let runtime = RehydrateFake::default();
    let applied = runtime.restored.clone();
    let ended = runtime.ended.clone();
    let restored_environment = runtime.restored_environments.clone();
    let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo.clone());

    restarted
        .ensure_session("sesn_running_rehydrate")
        .await
        .expect("C1 running Session remains recoverable");
    assert!(
        applied.lock().unwrap().is_empty(),
        "E2 no Resource re-apply"
    );
    assert!(ended.lock().unwrap().is_empty(), "E2 no terminal teardown");
    assert!(restored_environment.lock().unwrap().is_empty(), "E2");
    assert!(
        repo.get("sesn_running_rehydrate")
            .await
            .unwrap()
            .resources
            .has_active(),
        "E1 active generation preserved"
    );
}

#[test]
fn wire_projection_preserves_the_durable_execution_state() {
    // Session-status cause/effect decision table. C1 the durable activity
    // aggregate is running; C2 it is rescheduling; C3 it is idle; C4 it is
    // preparing/activating/failed; C5 it is terminal. E1 public GET reports running; E2 reports rescheduling;
    // E3 reports idle; E4 maps internal nonterminal preparation to rescheduling
    // and internal activation failure to the official terminated state;
    // E5 reports terminated. Rules S1=C1=>E1, S2=C2=>E2, S3=C3=>E3,
    // S4=C4=>E4, S5=C5=>E5. Unknown durable values fail during decoding
    // instead of being projected as a healthy idle Session. This keeps
    // the durable Session aggregate as the sole status truth; the wire cache
    // owns no separate dispatch inference.
    for (rule, durable, public) in [
        ("S1", SessionExecutionState::Running, SessionStatus::Running),
        (
            "S2",
            SessionExecutionState::Rescheduling,
            SessionStatus::Rescheduling,
        ),
        ("S3", SessionExecutionState::Idle, SessionStatus::Idle),
        (
            "S4a",
            SessionExecutionState::Preparing,
            SessionStatus::Rescheduling,
        ),
        (
            "S4b",
            SessionExecutionState::Activating,
            SessionStatus::Rescheduling,
        ),
        (
            "S4c",
            SessionExecutionState::ActivationFailed,
            SessionStatus::Terminated,
        ),
        (
            "S5",
            SessionExecutionState::Terminated,
            SessionStatus::Terminated,
        ),
    ] {
        assert_eq!(ManagedState::wire_session_status(durable), public, "{rule}");
    }
}

#[tokio::test]
async fn terminal_root_fences_mcp_recovery_before_runtime_effects() {
    // Cause graph: repository index hit + MCP nonterminal + root terminal
    // -> skip realization. The lifecycle classification table lives on
    // PersistedSession; this integration rule proves the scanner consumes it.
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let mut failed = sample_persisted("sesn_failed_mcp");
    failed.execution = SessionExecutionState::ActivationFailed;
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, failed).await;
    assert_eq!(
        repo.reconcilable_sessions().await.unwrap().sessions.len(),
        1,
        "T1 indexed"
    );

    let runtime = RehydrateFake::default();
    let runtime_effects = runtime.restored_runtimes.clone();
    let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo);
    assert_eq!(
        restarted.reconcile_session_realizations().await,
        0,
        "T1 skip"
    );
    assert!(
        runtime_effects.lock().unwrap().is_empty(),
        "T1 terminal root creates no MCP Runtime effect"
    );
}

#[tokio::test]
async fn startup_mcp_recovery_adopts_the_durable_environment_before_staging() {
    // Startup MCP recovery cause/effect graph:
    // C1 a live durable Session has a resident Environment binding; C2 its
    // active MCP generation needs process-local reconciliation after restart;
    // C3 the Runtime has an empty cache. E1 install the frozen projection;
    // E2 adopt the exact binding before MCP stage; E3 preserve that binding;
    // E4 do not open transcript/history as a side effect of the reconciler.
    //
    // Decision table:
    // | live root | binding | MCP recovery | result |
    // | yes | present | required | prepare -> adopt -> stage; binding unchanged |
    // | yes | absent  | required | prepare -> stage may create one Environment |
    // | terminal | any | required | no Runtime effect (covered above) |
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let mut persisted = sample_persisted("sesn_startup_mcp");
    persisted.environment.set_resident("durable-k8s-binding");
    create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, persisted).await;

    let runtime = RehydrateFake::default();
    let order = runtime.order.clone();
    let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo.clone());

    assert_eq!(restarted.reconcile_session_realizations().await, 1, "C1-C3");
    assert_eq!(
        order.lock().unwrap().as_slice(),
        &["runtime", "environment", "mcp"],
        "E1/E2/E4"
    );
    assert_eq!(
        repo.get("sesn_startup_mcp")
            .await
            .unwrap()
            .environment
            .binding(),
        Some("durable-k8s-binding"),
        "E3"
    );
}

#[tokio::test]
async fn live_file_attach_and_delete_survive_restart_without_projection_truth() {
    // Cause graph:
    // typed add/update/delete command -> prepare exact next generation
    // -> Runtime applies it -> aggregate commits it -> restart replays only
    // the committed generation. A projection is never persisted as truth.
    //
    // Decision table:
    // | Rule | Command | Runtime | Durable state | Restart behavior |
    // | R1 | add | success | revision +1, one Active | exact id/path restored |
    // | R2 | delete | success | revision +1, no Active | resource absent |
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
    let catalog = Arc::new(ephemeral_resource_registry());
    let state = ManagedState::new_with_mcp(RehydrateFake::default())
        .with_session_repo(repo.clone())
        .with_resource_registry(catalog.clone());
    let id = state
        .create_session(bare_create_params(), None)
        .await
        .expect("create")
        .id;

    let resource = state
        .create_resource(
            &id,
            serde_json::from_value(serde_json::json!({
                "type": "file",
                "file_id": "immutable-file-hash",
                "mount_path": "/input.txt"
            }))
            .unwrap(),
        )
        .await
        .expect("attach");
    let resource_id = resource.id().unwrap().to_string();
    let mut after_attach = repo.get(&id).await.unwrap();
    assert_eq!(after_attach.resources.revision, 2);
    assert_eq!(
        after_attach
            .resources
            .activations
            .iter()
            .filter(|activation| {
                activation.state == awaken_session_contract::ActivationState::Active
            })
            .count(),
        1
    );
    // Crash recovery may replace a process-local physical owner only after
    // its lease expires. Expire the fixture explicitly; a still-live owner
    // is the fail-closed row covered by the realization ownership table.
    after_attach
        .realization
        .as_mut()
        .expect("local realization lease")
        .expires_at_unix_ms = 0;
    state
        .commit_session_snapshot(DEFAULT_SCOPE, after_attach, "expire-test-owner", Vec::new())
        .await
        .expect("expire the crashed Runtime owner");

    let restarted = ManagedState::new_with_mcp(RehydrateFake::default())
        .with_session_repo(repo.clone())
        .with_resource_registry(catalog.clone());
    restarted.ensure_session(&id).await.expect("rehydrate");
    let restored = restarted.list_resources(&id).expect("list restored");
    assert_eq!(restored.len(), 1);
    assert!(matches!(
        &restored[0],
        crate::types::resource::SessionResource::File { id, mount_path, .. }
            if id == &resource_id && mount_path == "/input.txt"
    ));

    restarted
        .delete_resource(&id, &resource_id)
        .await
        .expect("detach");
    let mut after_delete = repo.get(&id).await.unwrap();
    assert_eq!(after_delete.resources.revision, 3);
    assert!(
        after_delete
            .resources
            .activations
            .iter()
            .all(|activation| activation.state != awaken_session_contract::ActivationState::Active)
    );
    after_delete
        .realization
        .as_mut()
        .expect("replacement realization lease")
        .expires_at_unix_ms = 0;
    restarted
        .commit_session_snapshot(
            DEFAULT_SCOPE,
            after_delete,
            "expire-second-test-owner",
            Vec::new(),
        )
        .await
        .expect("expire the second crashed Runtime owner");
    let second_restart = ManagedState::new_with_mcp(RehydrateFake::default())
        .with_session_repo(repo)
        .with_resource_registry(catalog);
    second_restart
        .ensure_session(&id)
        .await
        .expect("rehydrate after delete");
    assert!(second_restart.list_resources(&id).unwrap().is_empty());
}
